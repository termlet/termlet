//! 传输：内存 SFTP 服务端，不需要测试服务器。

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use russh_sftp::protocol::StatusCode;

use super::memfs::MemFs;
use super::*;
use crate::sftp::error::Side;
use crate::sftp::transfer::Phase;
use crate::sftp::transfer::download::commit_part;
use crate::sftp::{
    ChmodRequest, DownloadRequest, EntryKind, ManagerOptions, ResumeOptions, SftpError,
    UploadRequest,
};

fn up(local: &Path, dir: &str, overwrite: bool) -> UploadRequest {
    UploadRequest {
        local_paths: vec![local.to_path_buf()],
        remote_dir: dir.into(),
        overwrite,
    }
}

fn down(remote: &str, dir: &Path) -> DownloadRequest {
    DownloadRequest {
        remote_paths: vec![remote.into()],
        local_dir: dir.to_path_buf(),
    }
}

async fn upload_and_wait(
    m: &SftpManager,
    s: &SftpSession,
    local: &Path,
    dir: &str,
    overwrite: bool,
) -> TransferView {
    let views = m.upload(s, up(local, dir, overwrite)).await.unwrap();
    wait_finished(m, &views[0].id).await
}

async fn download_and_wait(
    m: &SftpManager,
    s: &SftpSession,
    remote: &str,
    dir: &Path,
) -> TransferView {
    let views = m.download(s, down(remote, dir)).await.unwrap();
    wait_finished(m, &views[0].id).await
}

// 并发写入时服务器先写了后面的块、前面的块失败：临时文件中间有空洞，长度不可信。失败时把临时文件截到确认写入的长度，
// 继续时从那里接着传，内容完整（不能从临时文件的长度接着传，中间会留下一段全零）。
#[test]
fn resume_after_out_of_order_write_failure() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(1 << 20);
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("big.bin");
        std::fs::write(&local, &data).unwrap();
        const FAIL_AT: u64 = 64 << 10;
        fs.fail_write(FAIL_AT);
        let failed = upload_and_wait(&m, &s, &local, "/", false).await;
        assert!(
            failed.state == TransferState::Failed && failed.resumable,
            "写入失败后应失败并可继续：{failed:?}"
        );
        assert_eq!(fs.size("/big.bin.termlet-part"), Some(FAIL_AT));
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &failed.id).await;
        assert!(
            v.state == TransferState::Done && v.done == data.len() as u64,
            "{v:?}"
        );
        assert_eq!(fs.read("/big.bin").unwrap(), data);
        m.close_all().await;
    });
}

// 来不及截断临时文件时（例如连接已断开，重连后自动继续）：继续时从确认写入的位置接着传，不信任临时文件的长度。
#[test]
fn resume_after_out_of_order_write_no_truncate() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(1 << 20);
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("big.bin");
        std::fs::write(&local, &data).unwrap();
        const FAIL_AT: u64 = 64 << 10;
        fs.fail_write(FAIL_AT);
        fs.set_no_truncate(true);
        let failed = upload_and_wait(&m, &s, &local, "/", false).await;
        assert!(failed.state == TransferState::Failed && failed.resumable);
        assert!(
            fs.size("/big.bin.termlet-part").unwrap() > FAIL_AT,
            "前提：服务器已写了后面的块，临时文件中间有空洞"
        );
        fs.set_no_truncate(false);
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        assert_eq!(
            wait_finished(&m, &failed.id).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/big.bin").unwrap(), data);
        m.close_all().await;
    });
}

// 同时下载同名文件到同一目录：各自占用不同的名称，不共用临时文件，都能完成。
#[test]
fn concurrent_downloads_same_name() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        // 两个连接：一个的读取停住时不影响另一个
        let s1 = open_mem(&m, &fs, "w1").await;
        let s2 = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(512 << 10);
        fs.put("/nginx.conf", &data);
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        let release = fs.hold_next_read();
        let a = m.download(&s1, down("/nginx.conf", out)).await.unwrap();
        let part = out.join("nginx.conf.termlet-part");
        wait_until_async("临时文件出现", || part.exists()).await;
        let vb = download_and_wait(&m, &s2, "/nginx.conf", out).await;
        release.release();
        let va = wait_finished(&m, &a[0].id).await;
        assert!(
            va.state == TransferState::Done
                && vb.state == TransferState::Done
                && va.local != vb.local,
            "两个下载都应完成，保存到不同的文件：{va:?} {vb:?}"
        );
        for name in ["nginx.conf", "nginx (1).conf"] {
            assert_eq!(std::fs::read(out.join(name)).unwrap(), data, "{name}");
        }
        assert!(local_parts(out).is_empty());
        m.close_all().await;
    });
}

// 本机已有同名的临时文件（另一个任务失败后留着供继续）：新的下载改名保存，不截断它。
#[test]
fn download_keeps_others_part() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/app.log", b"new content");
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        let stale = out.join("app.log.termlet-part");
        std::fs::write(&stale, "partial").unwrap();
        let v = download_and_wait(&m, &s, "/app.log", out).await;
        assert!(
            v.state == TransferState::Done && v.local.file_name().unwrap() == "app (1).log",
            "应改名保存为 app (1).log：{v:?}"
        );
        assert_eq!(std::fs::read_to_string(&stale).unwrap(), "partial");
        assert_eq!(
            std::fs::read_to_string(out.join("app (1).log")).unwrap(),
            "new content"
        );
        m.close_all().await;
    });
}

// 服务器把单次读取限制在请求的长度以下（又没有声明上限）：只补缺口，内容完整，请求数不会成倍增加
#[test]
fn download_with_capped_reads() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(1 << 20);
        fs.put("/big.bin", &data);
        fs.cap_reads(Some(20_000));
        let out = tempfile::tempdir().unwrap();
        let before = fs.read_requests();
        let v = download_and_wait(&m, &s, "/big.bin", out.path()).await;
        assert_eq!(v.state, TransferState::Done, "{v:?}");
        assert_eq!(std::fs::read(out.path().join("big.bin")).unwrap(), data);
        // 每个 32 KiB 的请求要两次（一次补缺口），加上末尾在途的请求
        let requests = fs.read_requests() - before;
        assert!(
            requests < 3 * ((1 << 20) / 20_000),
            "读取请求过多：{requests}"
        );
        m.close_all().await;
    });
}

// 下载没有读权限的文件（可以查看属性、打开时没有权限）：失败时不留下空的临时文件、不显示为可继续，之后再下载同一个文件时
// 仍保存为原来的名称；继续这个任务时从头传，重新选定名称。
#[test]
fn download_unreadable_leaves_no_part() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/syslog", b"log");
        fs.deny_read("/syslog", true);
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        let failed = download_and_wait(&m, &s, "/syslog", out).await;
        assert!(
            failed.state == TransferState::Failed
                && matches!(
                    failed.error,
                    Some(SftpError::PermissionDenied {
                        side: Side::Remote,
                        ..
                    })
                )
                && !failed.resumable
                && failed.local == out,
            "应失败为没有权限、不可继续，不占用名称：{failed:?}"
        );
        assert!(local_names(out).is_empty());

        fs.deny_read("/syslog", false);
        let again = download_and_wait(&m, &s, "/syslog", out).await;
        assert!(again.state == TransferState::Done && again.local.file_name().unwrap() == "syslog");
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &failed.id).await;
        assert!(
            v.state == TransferState::Done && v.local.file_name().unwrap() == "syslog (1)",
            "继续时应重新选定名称：{v:?}"
        );
        assert_eq!(local_names(out), ["syslog", "syslog (1)"]);
        m.close_all().await;
    });
}

// 旧的记录被挤出（每个窗口只保留最近的若干条）时，失败留下的临时文件一并删除。
#[test]
fn prune_removes_part() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            keep_finished: 1,
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/big.bin", &random_bytes(256 << 10));
        fs.put("/small.txt", b"x");
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        fs.fail_read(Some(128 << 10));
        let v = download_and_wait(&m, &s, "/big.bin", out).await;
        assert!(v.state == TransferState::Failed && v.resumable, "{v:?}");
        fs.fail_read(None);
        let part = out.join("big.bin.termlet-part");
        assert!(
            std::fs::metadata(&part).unwrap().len() > 0,
            "前提：留下了写了一部分的临时文件"
        );
        download_and_wait(&m, &s, "/small.txt", out).await;
        assert!(part.exists(), "记录还在时应保留临时文件");
        download_and_wait(&m, &s, "/small.txt", out).await;
        assert!(!part.exists(), "记录被挤出后应删除临时文件");
        m.close_all().await;
    });
}

// 清除记录时先删本机的临时文件再删远程的：删除远程的在连接卡住时会一直等，不应耽误本机的清理。
#[test]
fn clear_removes_local_parts_first() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(256 << 10);
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("up.bin");
        std::fs::write(&local, &data).unwrap();
        fs.put("/down.bin", &data);
        let out = tempfile::tempdir().unwrap();
        let out = out.path().to_path_buf();

        fs.fail_write(64 << 10);
        let v = upload_and_wait(&m, &s, &local, "/", false).await;
        assert!(v.state == TransferState::Failed && v.resumable, "{v:?}");
        fs.fail_read(Some(128 << 10));
        let v = download_and_wait(&m, &s, "/down.bin", &out).await;
        assert!(v.state == TransferState::Failed && v.resumable, "{v:?}");
        fs.fail_read(None);

        let release = fs.hold_removes();
        let clearing = {
            let m = m.clone();
            crate::runtime::spawn(async move { m.clear(Default::default()).await })
        };
        let part = out.join("down.bin.termlet-part");
        wait_until_async(
            "删除远程的临时文件停住时，本机的临时文件也应已删除",
            || !part.exists(),
        )
        .await;
        release.release();
        let n = tokio::time::timeout(Duration::from_secs(10), clearing)
            .await
            .expect("清除记录超时")
            .unwrap();
        assert_eq!(n, 2);
        assert!(!fs.exists("/up.bin.termlet-part"), "远程的临时文件也应删除");
        m.close_all().await;
    });
}

// 下载期间有别的程序（或用户、rz）在最终名称上建了文件：不覆盖它，改名保存，结果中是最终的路径。
#[test]
fn download_keeps_file_created_meanwhile() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/app.log", b"remote");
        fs.mkdir("/d");
        fs.put("/d/a.txt", b"remote");
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        for (remote, final_name, renamed, local) in [
            ("/app.log", "app.log", "app (1).log", "app (1).log"),
            // 目录中的文件：改名保存在目录中，结果中仍是目录
            ("/d", "d/a.txt", "d/a (1).txt", "d"),
        ] {
            let release = fs.hold_next_read();
            let views = m.download(&s, down(remote, out)).await.unwrap();
            let final_path = out.join(final_name);
            let part = final_path.with_file_name(format!(
                "{}.termlet-part",
                final_path.file_name().unwrap().to_string_lossy()
            ));
            wait_until_async("临时文件出现", || part.exists()).await;
            std::fs::write(&final_path, "mine").unwrap();
            release.release();
            let v = wait_finished(&m, &views[0].id).await;
            assert!(
                v.state == TransferState::Done && v.local == out.join(local),
                "{remote}：应改名保存为 {local}：{v:?}"
            );
            assert_eq!(std::fs::read_to_string(&final_path).unwrap(), "mine");
            assert_eq!(
                std::fs::read_to_string(out.join(renamed)).unwrap(),
                "remote"
            );
        }
        assert_eq!(local_names(out), ["app (1).log", "app.log", "d"]);
        assert_eq!(local_names(&out.join("d")), ["a (1).txt", "a.txt"]);
        m.close_all().await;
    });
}

// 改名不覆盖已有的文件；有别的任务占用（有它的临时文件）的名称也跳过；命令行工具的下载（覆盖）仍按 scp 的语义覆盖。
#[test]
fn commit_part_no_replace() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let part = dir.join("x.termlet-part");
    let local = dir.join("x");
    std::fs::write(&local, "old").unwrap();
    std::fs::write(&part, "new").unwrap();
    std::fs::write(dir.join("x (1).termlet-part"), "other").unwrap();
    assert_eq!(
        commit_part(false, &part, &local).unwrap(),
        dir.join("x (2)")
    );
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "old");
    std::fs::write(&part, "newest").unwrap();
    assert_eq!(commit_part(true, &part, &local).unwrap(), local);
    assert_eq!(std::fs::read_to_string(&local).unwrap(), "newest");
}

// 选定下载的位置并占用
#[test]
fn reserve_local_names() {
    use crate::sftp::transfer::download::reserve_local;
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    for name in ["a.txt", "a (1).txt", ".bashrc", "b.txt.termlet-part"] {
        std::fs::write(dir.join(name), "").unwrap();
    }
    let reserve = |name: &str, is_dir: bool| {
        let (path, _) = reserve_local(&dir.join(name), is_dir).unwrap();
        path.file_name().unwrap().to_string_lossy().into_owned()
    };
    // 文件：占用临时文件；同名的临时文件（别的任务的）也算重名
    assert_eq!(reserve("a.txt", false), "a (2).txt");
    assert!(
        dir.join("a (2).txt.termlet-part").exists(),
        "应占用临时文件"
    );
    assert_eq!(reserve("a.txt", false), "a (3).txt");
    assert_eq!(reserve(".bashrc", false), ".bashrc (1)");
    assert_eq!(reserve("b.txt", false), "b (1).txt");
    // 目录：新建目录占用；文件任务已占用的名称（有它的临时文件）也算重名
    assert_eq!(reserve("a.txt", true), "a (4).txt");
    assert!(dir.join("a (4).txt").is_dir());
}

// 本机根路径是指向目录的链接（Windows 的目录联接、符号链接）：上传它指向的目录中的文件；子项中的链接不跟随。
#[test]
fn upload_linked_directory() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let base = tempfile::tempdir().unwrap();
        let real = base.path().join("realDir");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        std::fs::write(real.join("a.txt"), "a").unwrap();
        std::fs::write(real.join("sub").join("b.txt"), "b").unwrap();
        let link = base.path().join("linked");
        link_dir(&real, &link);
        link_dir(&real, &real.join("loop"));
        let v = upload_and_wait(&m, &s, &link, "/", false).await;
        assert!(
            v.state == TransferState::Done && v.files == 2 && v.files_done == 2,
            "应上传链接指向的目录中的 2 个文件：{v:?}"
        );
        assert_eq!(fs.read("/linked/a.txt").unwrap(), b"a");
        assert_eq!(fs.read("/linked/sub/b.txt").unwrap(), b"b");
        assert!(!fs.exists("/linked/loop"), "子项中的链接不应跟随");
        m.close_all().await;
    });
}

// 远程根路径是指向目录的符号链接：下载它指向的目录，递归修改权限时修改其中的文件与子目录；子项中的链接不跟随。
#[test]
fn linked_remote_directory() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.mkdir("/dir");
        fs.mkdir("/dir/realDir");
        fs.mkdir("/dir/realDir/sub");
        fs.put("/dir/realDir/a.txt", b"a");
        fs.put("/dir/realDir/sub/b.txt", b"b");
        fs.symlink("/dir/realDir", "/dir/linked");
        fs.symlink("/dir/realDir", "/dir/realDir/loop");
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        let v = download_and_wait(&m, &s, "/dir/linked", out).await;
        assert!(
            v.state == TransferState::Done && v.files == 2 && v.files_done == 2,
            "应下载链接指向的目录中的 2 个文件：{v:?}"
        );
        assert_eq!(std::fs::read(out.join("linked/a.txt")).unwrap(), b"a");
        assert_eq!(std::fs::read(out.join("linked/sub/b.txt")).unwrap(), b"b");
        assert!(
            std::fs::symlink_metadata(out.join("linked/loop")).is_err(),
            "子项中的链接不应跟随"
        );

        s.chmod(&ChmodRequest {
            paths: vec!["/dir/linked".into()],
            mode: 0o600,
            recursive: true,
            ..Default::default()
        })
        .await
        .unwrap();
        for (p, want) in [
            ("/dir/realDir", 0o700),
            ("/dir/realDir/a.txt", 0o600),
            ("/dir/realDir/sub", 0o700),
            ("/dir/realDir/sub/b.txt", 0o600),
        ] {
            assert_eq!(fs.mode(p), Some(want), "{p}");
        }
        m.close_all().await;
    });
}

// 同时继续同一个失败的任务（断线重连后的自动继续与用户点"继续"同时发生）：只启动一次。继续后还在排队时取消：不再运行。
#[test]
fn resume_transfer_once() {
    block_on(async {
        let m = SftpManager::new(ManagerOptions {
            per_session: 1,
            ..options()
        });
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        // 占住唯一的传输位置：继续的任务都停在排队中
        let slot = s.slots.clone().acquire_owned().await.unwrap();
        let missing = tempfile::tempdir().unwrap();
        let missing = missing.path().join("none");
        let failed = || async {
            let t =
                m.0.add_transfer(
                    &s,
                    crate::sftp::Direction::Upload,
                    "x".into(),
                    missing.clone(),
                    "/x".into(),
                    false,
                    false,
                )
                .await;
            t.lock().phase = Phase::Failed;
            t
        };
        for i in 0..200 {
            let id = failed().await.id.clone();
            let ok = Arc::new(AtomicUsize::new(0));
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    let m = m.clone();
                    let id = id.clone();
                    let ok = ok.clone();
                    std::thread::spawn(move || {
                        if m.resume(&id, ResumeOptions::default()).is_ok() {
                            ok.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            assert_eq!(
                ok.load(Ordering::SeqCst),
                1,
                "第 {i} 次：同一任务应只继续一次"
            );
        }

        let t = failed().await;
        t.lock().cancel = Some(crate::sftp::cancel::Cancel::new()); // 上一次运行留下的
        m.resume(&t.id, ResumeOptions::default()).unwrap();
        m.cancel(&t.id).unwrap();
        wait_view(&m, &t.id, |v| v.state == TransferState::Canceled).await;
        drop(slot);
        m.close_all().await;
    });
}

// 列目录时符号链接的权限取它指向的目标的（修改权限作用于目标，对话框的初始值应与目标一致）；悬空链接保持链接本身的。
#[test]
fn list_link_perm_from_target() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/key", b"secret");
        fs.symlink("/key", "/key-link");
        fs.symlink("/none", "/dangling");
        let list = s.list("/").await.unwrap();
        assert_eq!(list.path, "/");
        let find = |name: &str| {
            list.entries
                .iter()
                .find(|e| e.name == name)
                .unwrap()
                .clone()
        };
        let link = find("key-link");
        assert_eq!(
            (link.perm, link.kind, link.target),
            (0o644, EntryKind::Symlink, Some(EntryKind::File))
        );
        assert_eq!(link.mode, "lrwxrwxrwx");
        let dangling = find("dangling");
        assert_eq!((dangling.perm, dangling.target), (0o777, None));
        assert!(!dangling.is_dir());
        let key = find("key");
        assert_eq!(
            (key.size, key.mode.as_str(), key.uid),
            (6, "-rw-r--r--", Some(1000))
        );
        m.close_all().await;
    });
}

// 上传完成后改名为正式文件失败（posix-rename 没有权限，普通的改名也失败）：远程原有的文件保留，任务失败、可以继续，
// 继续时只需重新改名。只有服务器不支持 posix-rename 时才经备份替换。
#[test]
fn upload_rename_failure_keeps_original() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.put("/conf.txt", b"old");
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("conf.txt");
        std::fs::write(&local, "new content").unwrap();

        fs.fail_rename(
            Some(StatusCode::PermissionDenied),
            Some(StatusCode::Failure),
        );
        let failed = upload_and_wait(&m, &s, &local, "/", true).await;
        assert!(
            failed.state == TransferState::Failed
                && matches!(failed.error, Some(SftpError::PermissionDenied { .. }))
                && failed.resumable,
            "改名失败时应失败为没有权限、可以继续：{failed:?}"
        );
        assert_eq!(fs.read("/conf.txt").unwrap(), b"old");
        fs.fail_rename(None, None);
        m.resume(&failed.id, ResumeOptions::default()).unwrap();
        assert_eq!(
            wait_finished(&m, &failed.id).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/conf.txt").unwrap(), b"new content");

        fs.fail_rename(Some(StatusCode::OpUnsupported), None);
        std::fs::write(&local, "newer").unwrap();
        let v = upload_and_wait(&m, &s, &local, "/", true).await;
        assert_eq!(v.state, TransferState::Done, "{v:?}");
        assert_eq!(fs.read("/conf.txt").unwrap(), b"newer");
        assert!(remote_parts(&fs, "/").is_empty());
        m.close_all().await;
    });
}

// 两个窗口同时上传同名文件到同一个远程目录：各用各的临时文件，都能完成，先完成的不会把另一个的临时文件改名走；
// 最后的文件是后完成的那一个，内容完整。
#[test]
fn concurrent_uploads_same_name() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s1 = open_mem(&m, &fs, "w1").await;
        let s2 = open_mem(&m, &fs, "w2").await;
        let (a, b) = (random_bytes(256 << 10), random_bytes(256 << 10));
        let (ta, tb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (la, lb) = (ta.path().join("same.bin"), tb.path().join("same.bin"));
        std::fs::write(&la, &a).unwrap();
        std::fs::write(&lb, &b).unwrap();
        let hold = fs.hold_next_write();
        let va = m.upload(&s1, up(&la, "/", true)).await.unwrap();
        wait_until_async("临时文件出现", || fs.exists("/same.bin.termlet-part")).await;
        let vb = upload_and_wait(&m, &s2, &lb, "/", true).await;
        assert_eq!(vb.state, TransferState::Done, "后开始的上传应完成：{vb:?}");
        assert_eq!(fs.read("/same.bin").unwrap(), b);
        assert!(
            fs.exists("/same.bin.termlet-part"),
            "另一个任务的临时文件不应被改名走"
        );
        hold.release();
        assert_eq!(
            wait_finished(&m, &va[0].id).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/same.bin").unwrap(), a);
        assert!(remote_parts(&fs, "/").is_empty());
        m.close_all().await;
    });
}

// 继续失败的上传时沿用它占用的临时文件；没有任务占用的临时文件（以前留下的）删除后重新创建，不另起名称。
#[test]
fn remote_part_claim() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let data = random_bytes(256 << 10);
        let tmp = tempfile::tempdir().unwrap();
        let local = tmp.path().join("big.bin");
        std::fs::write(&local, &data).unwrap();
        fs.put("/stale.txt.termlet-part", b"left over");
        let stale = tmp.path().join("stale.txt");
        std::fs::write(&stale, "fresh").unwrap();

        fs.fail_write(64 << 10);
        let first = upload_and_wait(&m, &s, &local, "/", true).await;
        assert!(
            first.state == TransferState::Failed && first.resumable,
            "{first:?}"
        );
        // 失败的任务占着"big.bin.termlet-part"：新的上传改用"big.bin.1.termlet-part"
        assert_eq!(
            upload_and_wait(&m, &s, &local, "/", true).await.state,
            TransferState::Done
        );
        assert!(fs.exists("/big.bin.termlet-part"));
        m.resume(&first.id, ResumeOptions::default()).unwrap();
        let v = wait_finished(&m, &first.id).await;
        assert!(
            v.state == TransferState::Done && v.done == data.len() as u64,
            "{v:?}"
        );
        assert_eq!(fs.read("/big.bin").unwrap(), data);
        assert_eq!(
            upload_and_wait(&m, &s, &stale, "/", true).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/stale.txt").unwrap(), b"fresh");
        assert!(
            remote_parts(&fs, "/").is_empty(),
            "不应残留临时文件（以前留下的应被沿用）"
        );
        m.close_all().await;
    });
}

// 覆盖 0600 的文件时临时文件一创建就是 0600；别人预先放好的同名符号链接不会被写进去
#[test]
fn remote_part_is_private_and_not_followed() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let tmp = tempfile::tempdir().unwrap();
        fs.put("/.env", b"OLD=1");
        s.client()
            .setstat("/.env", crate::sftp::client::perm_attrs(0o600))
            .await
            .unwrap();
        let local = tmp.path().join(".env");
        std::fs::write(&local, "NEW=2").unwrap();
        let hold = fs.hold_next_write();
        let view = m.upload(&s, up(&local, "/", true)).await.unwrap();
        wait_until_async("临时文件出现", || fs.exists("/.env.termlet-part")).await;
        assert_eq!(fs.mode("/.env.termlet-part"), Some(0o600));
        hold.release();
        assert_eq!(
            wait_finished(&m, &view[0].id).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/.env").unwrap(), b"NEW=2");
        assert_eq!(fs.mode("/.env"), Some(0o600));

        fs.put("/victim", b"keep me");
        fs.symlink("/victim", "/app.conf.termlet-part");
        let local = tmp.path().join("app.conf");
        std::fs::write(&local, "conf").unwrap();
        assert_eq!(
            upload_and_wait(&m, &s, &local, "/", true).await.state,
            TransferState::Done
        );
        assert_eq!(fs.read("/app.conf").unwrap(), b"conf");
        assert_eq!(fs.read("/victim").unwrap(), b"keep me");
        assert!(remote_parts(&fs, "/").is_empty());
        m.close_all().await;
    });
}

// 文件名接近上限、加上临时文件的后缀后超过 255 字节（或 Windows 的 255 个 UTF-16 单位）时改用短的临时文件名，上传、下载都能完成。
#[test]
fn long_name_parts() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        for name in [
            format!("{}.pdf", "a".repeat(246)),
            format!("{}.pdf", "文".repeat(82)),
        ] {
            let data = format!("content of {name}").into_bytes();
            let tmp = tempfile::tempdir().unwrap();
            let local = tmp.path().join(&name);
            std::fs::write(&local, &data).unwrap();
            let v = upload_and_wait(&m, &s, &local, "/", false).await;
            assert_eq!(
                v.state,
                TransferState::Done,
                "上传 {} 字节的文件名应完成：{v:?}",
                name.len()
            );
            assert_eq!(fs.read(&format!("/{name}")).unwrap(), data);
            let out = tempfile::tempdir().unwrap();
            let v = download_and_wait(&m, &s, &format!("/{name}"), out.path()).await;
            assert_eq!(
                v.state,
                TransferState::Done,
                "下载 {} 字节的文件名应完成：{v:?}",
                name.len()
            );
            assert_eq!(std::fs::read(out.path().join(&name)).unwrap(), data);
        }
        m.close_all().await;
    });
}

// 下载目录时没有读权限的子目录与文件跳过（记在结果中），其余照常传完；所选的目录本身读不了时照常失败。
#[test]
fn download_directory_skips_unreadable() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        fs.mkdir("/logs");
        fs.mkdir("/logs/private");
        fs.put("/logs/a.log", b"a");
        fs.put("/logs/btmp", b"secret");
        fs.put("/logs/private/x.log", b"x");
        fs.deny_read("/logs/private", true);
        fs.deny_read("/logs/btmp", true);
        let out = tempfile::tempdir().unwrap();
        let out = out.path();
        let mut v = download_and_wait(&m, &s, "/logs", out).await;
        v.skipped_paths.sort();
        assert!(
            v.state == TransferState::Done
                && v.skipped == 2
                && v.skipped_paths == ["/logs/btmp", "/logs/private"]
                && (v.files, v.files_done, v.total, v.done) == (1, 1, 1, 1),
            "应跳过读不了的子目录与文件，其余传完：{v:?}"
        );
        assert_eq!(std::fs::read(out.join("logs/a.log")).unwrap(), b"a");
        assert!(!out.join("logs/btmp").exists(), "读不了的文件不应下载");

        fs.deny_read("/logs", true);
        let v = download_and_wait(&m, &s, "/logs", out).await;
        assert!(
            v.state == TransferState::Failed
                && matches!(v.error, Some(SftpError::PermissionDenied { .. })),
            "所选的目录读不了时应失败：{v:?}"
        );
        m.close_all().await;
    });
}

// 上传目录时本机没有读权限的子目录与文件跳过（记在结果中），其余照常传完。
#[test]
fn upload_directory_skips_unreadable() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(tree.join("private")).unwrap();
        std::fs::write(tree.join("a.txt"), "a").unwrap();
        std::fs::write(tree.join("secret.txt"), "secret").unwrap();
        std::fs::write(tree.join("private").join("x.txt"), "x").unwrap();
        let Some(_private) = Denied::new(&tree.join("private")) else {
            return;
        };
        let Some(_secret) = Denied::new(&tree.join("secret.txt")) else {
            return;
        };
        let mut v = upload_and_wait(&m, &s, &tree, "/", false).await;
        v.skipped_paths.sort();
        let want = [
            tree.join("private").display().to_string(),
            tree.join("secret.txt").display().to_string(),
        ];
        assert!(
            v.state == TransferState::Done
                && v.skipped == 2
                && v.skipped_paths == want
                && (v.files, v.files_done) == (1, 1),
            "应跳过读不了的子目录与文件，其余传完：{v:?}"
        );
        assert_eq!(fs.read("/tree/a.txt").unwrap(), b"a");
        assert!(!fs.exists("/tree/secret.txt"), "读不了的文件不应上传");
        m.close_all().await;
    });
}

// 窗口关闭后它的传输记录再也用不上：关闭会话前删除这些记录与远程的临时文件；进行中的上传被取消时，也在连接关闭前
// 删除它的远程临时文件。别的窗口的会话与记录不受影响。
#[test]
fn close_owner_removes_remote_parts() {
    block_on(async {
        let m = manager();
        let fs = MemFs::new();
        let s = open_mem(&m, &fs, "w1").await;
        // 进行中的上传用另一个连接
        let busy = open_mem(&m, &fs, "w1").await;
        let other = open_mem(&m, &fs, "w2").await;
        let tmp = tempfile::tempdir().unwrap();
        let failed_local = tmp.path().join("failed.bin");
        let running_local = tmp.path().join("running.bin");
        std::fs::write(&failed_local, random_bytes(256 << 10)).unwrap();
        std::fs::write(&running_local, random_bytes(8 << 20)).unwrap();

        fs.fail_write(64 << 10);
        let v = upload_and_wait(&m, &s, &failed_local, "/", false).await;
        assert!(v.state == TransferState::Failed && v.resumable, "{v:?}");
        // 远程没有 /w2 目录，失败
        upload_and_wait(&m, &other, &failed_local, "/w2", false).await;

        let hold = fs.hold_next_write();
        let running = m
            .upload(&busy, up(&running_local, "/", false))
            .await
            .unwrap();
        wait_until_async("临时文件出现", || {
            fs.exists("/running.bin.termlet-part")
        })
        .await;
        let closing = {
            let m = m.clone();
            crate::runtime::spawn(async move { m.close_owner(Some("w1")).await })
        };
        // 会话已从列表中去掉（随即被取消）后再放开写入：上传在取消后删除它的临时文件，之后才关闭连接
        wait_until_async("关闭会话", || m.session(busy.id()).is_none()).await;
        hold.release();
        let n = tokio::time::timeout(Duration::from_secs(10), closing)
            .await
            .expect("关闭会话超时")
            .unwrap();
        assert_eq!(n, 2);
        assert!(
            !fs.exists("/running.bin"),
            "进行中的上传 {} 应被取消",
            running[0].id
        );
        assert!(
            remote_parts(&fs, "/").is_empty(),
            "不应残留远程的临时文件：{:?}",
            fs.names("/")
        );
        assert!(
            m.transfers(Some("w1")).is_empty(),
            "关闭的窗口的传输记录应删除"
        );
        assert_eq!(m.transfers(Some("w2")).len(), 1, "别的窗口的记录不受影响");
        assert!(m.session(other.id()).is_some(), "别的窗口的会话不受影响");
        m.close_all().await;
    });
}
