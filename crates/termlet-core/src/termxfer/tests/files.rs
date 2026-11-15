//! 本机文件与管道：文件名处理、重名改名、列出上传的文件、管道。

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::files::{CollectError, Source, collect, create_unique, safe_name};
use super::super::pipe::{Pipe, PipeError, after_can_run, text_suffix};
use super::write_file;

#[test]
fn safe_names() {
    let cases: [(&[u8], &str); 10] = [
        (b"a.txt", "a.txt"),
        (b"../../etc/passwd", "passwd"),
        (br"..\..\win.ini", "win.ini"),
        (b"a<b>:c?.txt", "a_b__c_.txt"),
        (b"con.txt", "_con.txt"),
        (b"LPT1", "_LPT1"),
        (b"name. ", "name"),
        (b"..", "file"),
        (b"\x01bell\x7f.log", "bell.log"),
        // GBK
        (b"\xc4\xe3\xba\xc3.txt", "你好.txt"),
    ];
    for (input, want) in cases {
        assert_eq!(safe_name(input), want, "{input:?}");
    }
    let long = safe_name("长".repeat(200).as_bytes());
    assert!(long.len() <= 255, "文件名应截到 255 字节：{}", long.len());
    assert!(long.chars().all(|c| c == '长'), "按字符截断");
}

fn create(dir: &Path, name: &str, is_dir: bool) -> String {
    let (path, file) = create_unique(dir, name, is_dir).unwrap();
    assert_eq!(
        file.is_none(),
        is_dir,
        "新建文件时返回打开的文件，目录时为 None"
    );
    path.file_name().unwrap().to_string_lossy().into_owned()
}

fn rels(list: &[Source]) -> String {
    list.iter()
        .map(|s| s.rel.join("/"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn create_unique_and_collect() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("a.txt"), b"x");
    write_file(&dir.join("a (1).txt"), b"x");
    assert_eq!(create(dir, "a.txt", false), "a (2).txt", "重名时改名");
    assert_eq!(
        create(dir, "a.txt", false),
        "a (3).txt",
        "已新建的名称不应再用"
    );
    assert_eq!(create(dir, ".bashrc", false), ".bashrc", "点开头的文件");
    // 与已有的目录同名、与已有的文件同名的目录：都改名
    std::fs::create_dir(dir.join("d")).unwrap();
    assert_eq!(create(dir, "d", false), "d (1)", "与已有目录同名的文件");
    assert_eq!(
        create(dir, ".bashrc", true),
        ".bashrc (1)",
        "与已有文件同名的目录"
    );
    assert_eq!(
        super::read_file(&dir.join("a.txt")),
        b"x",
        "已有的文件不应改动"
    );

    let tree = dir.join("tree");
    write_file(&tree.join("x.txt"), b"x");
    write_file(&tree.join("sub").join("y.txt"), b"x");
    std::fs::create_dir_all(tree.join("empty")).unwrap();
    assert!(
        matches!(
            collect(std::slice::from_ref(&tree), false),
            Err(CollectError::Directory(_))
        ),
        "不允许目录时应报错"
    );
    let (list, skipped) = collect(&[dir.join("a.txt"), tree], true).unwrap();
    assert_eq!(
        rels(&list),
        "a.txt tree tree/empty tree/sub tree/sub/y.txt tree/x.txt",
        "展开目录"
    );
    assert!(skipped.is_empty());
    assert_eq!(list[0].id, 0);
    assert_eq!(list[1].id, 1);
    assert!(list[1].is_dir);
    assert_eq!(list[5].size, 1, "项的编号、目录与大小");
    // 不存在的文件
    assert!(matches!(
        collect(&[dir.join("missing")], true),
        Err(CollectError::Other(_))
    ));
}

/// 新建指向目录 `target` 的链接：Windows 上用 junction（不需要管理员权限或开发者模式），其他系统用符号链接。
fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(out.status.success(), "新建 junction 失败：{out:?}");
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

// 所选目录是指向目录的链接（Windows 的 junction、符号链接）：展开它指向的目录；子项中的链接不跟随（避免循环）。
#[test]
fn collect_linked_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let real = tmp.path().join("realDir");
    write_file(&real.join("x.txt"), b"x");
    write_file(&real.join("sub").join("y.txt"), b"x");
    let link = tmp.path().join("linked");
    link_dir(&real, &link);
    link_dir(&real, &real.join("loop"));
    let (list, _) = collect(&[link], true).unwrap();
    assert_eq!(
        rels(&list),
        "linked linked/sub linked/sub/y.txt linked/x.txt",
        "展开链接指向的目录"
    );
}

/// 让当前用户读不了 `p`（目录不能列出内容），返回恢复的操作。Windows 上用 icacls 加一条拒绝读取的权限；
/// 其他系统改权限为 000。不受权限限制时（Unix 的 root，启用了备份特权的 Windows 管理员，例如 GitHub 的运行器上
/// 列目录不看拒绝的权限）恢复原样，返回 `None` 跳过测试。
fn deny_local(p: &Path) -> Option<Box<dyn FnOnce()>> {
    #[cfg(windows)]
    let restore: Box<dyn FnOnce()> = {
        let user = std::env::var("USERNAME").ok()?;
        let out = std::process::Command::new("icacls")
            .arg(p)
            .args(["/deny", &format!("{user}:(RD)")])
            .output()
            .unwrap();
        assert!(out.status.success(), "icacls 拒绝读取失败：{out:?}");
        let p: PathBuf = p.to_path_buf();
        Box::new(move || {
            let _ = std::process::Command::new("icacls")
                .arg(&p)
                .args(["/remove:d", &user])
                .output();
        })
    };
    #[cfg(unix)]
    let restore: Box<dyn FnOnce()> = {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o000)).unwrap();
        let p: PathBuf = p.to_path_buf();
        Box::new(move || {
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700));
        })
    };
    if std::fs::read_dir(p).is_ok() {
        restore();
        println!(
            "{} 改了权限仍然读得到（以 root 或有备份特权的管理员运行），跳过",
            p.display()
        );
        return None;
    }
    Some(restore)
}

// trz -d 选的目录中有读不了的子目录：不展开它（它本身照常建出来），记为跳过，其余照常上传；所选的目录本身读不了时照常报错。
#[test]
fn collect_skips_unreadable() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_file(&tree.join("x.txt"), b"x");
    write_file(&tree.join("private").join("y.txt"), b"x");
    let Some(restore_private) = deny_local(&tree.join("private")) else {
        return;
    };
    let result = collect(std::slice::from_ref(&tree), true);
    let restore_tree = deny_local(&tree).unwrap();
    let denied_root = collect(std::slice::from_ref(&tree), true);
    restore_tree();
    restore_private();
    let (list, skipped) = result.unwrap();
    assert_eq!(rels(&list), "tree tree/private tree/x.txt");
    assert_eq!(skipped, ["tree/private"], "应跳过读不了的子目录");
    assert!(denied_root.is_err(), "所选的目录读不了时应报错");
}

#[test]
fn pipe_reads_and_returns_leftover() {
    let p = Pipe::new(16);
    p.push_blocking(b"hello".to_vec()).unwrap();
    let first = p.next(None).unwrap();
    assert_eq!(first, b"hello");
    // 协议只用了一部分：放回去
    p.unread(&first[3..]);
    p.push_blocking(b" world".to_vec()).unwrap();
    assert_eq!(p.close(), b"lo world", "关闭时取回剩下的");
    assert!(p.push_blocking(b"x".to_vec()).is_err(), "关闭后写入应退回");
    assert_eq!(p.next(None), Err(PipeError::Closed));
}

#[test]
fn pipe_timeout_cancel_and_stop() {
    let p = Pipe::new(1);
    let started = std::time::Instant::now();
    assert_eq!(
        p.next(Some(started + Duration::from_millis(50))),
        Err(PipeError::Timeout)
    );
    assert!(started.elapsed() >= Duration::from_millis(45));
    // 取消后读取立即返回，即使还有数据
    p.push_blocking(b"x".to_vec()).unwrap();
    p.cancel();
    assert_eq!(p.next(None), Err(PipeError::Cancelled));
    // 停止后不再接受写入，已缓冲的仍可取回
    p.stop();
    assert_eq!(p.push_blocking(b"y".to_vec()), Err(b"y".to_vec()));
    assert_eq!(p.close(), b"x");
}

#[test]
fn pipe_push_waits_for_space() {
    let p = std::sync::Arc::new(Pipe::new(1));
    p.push_blocking(b"a".to_vec()).unwrap();
    let writer = {
        let p = p.clone();
        std::thread::spawn(move || p.push_blocking(b"b".to_vec()))
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!writer.is_finished(), "缓冲满时写入应等待");
    assert_eq!(p.next(None).unwrap(), b"a");
    writer.join().unwrap().unwrap();
    assert_eq!(p.next(None).unwrap(), b"b");
    // 异步写入：缓冲满时等待，停止后退回
    p.push_blocking(b"c".to_vec()).unwrap();
    let rt = crate::runtime::runtime();
    let pending = {
        let p = p.clone();
        rt.spawn(async move { p.push(b"d".to_vec()).await })
    };
    std::thread::sleep(Duration::from_millis(50));
    assert!(!pending.is_finished());
    p.stop();
    assert_eq!(rt.block_on(pending).unwrap(), Err(b"d".to_vec()));
}

#[test]
fn drain_binary_returns_text_after_abort() {
    // lrzsz 中止时回送一串 CAN：其后是远端程序退出后的输出
    assert_eq!(
        after_can_run(b"ab\x18cd\x18\x18\x18\x18\x18\x18tail"),
        Some(11)
    );
    assert_eq!(after_can_run(b"\x18x\x18"), None);
    assert_eq!(
        text_suffix(b"\x00\x8a\xffok \xe4\xbd\xa0"),
        "ok 你".as_bytes()
    );

    let p = Pipe::new(16);
    p.push_blocking(b"\x00\x01binary\x18A".to_vec()).unwrap();
    p.push_blocking(b"\x02\x18\x18\x18\x18\x18\x18\x08\x08\x02\xff$ prompt".to_vec())
        .unwrap();
    p.drain_binary(Duration::from_millis(100), Duration::from_secs(2));
    assert_eq!(p.close(), b"$ prompt");

    // 没有 CAN，整段是文本：连同上一段末尾的文本一起交还
    let p = Pipe::new(16);
    p.push_blocking(b"\x00\x01\x02\x03junk\x10\x11\x12$ ls".to_vec())
        .unwrap();
    p.push_blocking(b"\r\nfile.txt\r\n".to_vec()).unwrap();
    p.drain_binary(Duration::from_millis(100), Duration::from_secs(2));
    assert_eq!(p.close(), b"$ ls\r\nfile.txt\r\n");

    // 远端不再输出：最后一段末尾的文本交还
    let p = Pipe::new(16);
    p.push_blocking(b"\x00\x01\x02\x03\x04\x05\x06\x0e\x0f\x10\x11\x12\x13done$ ".to_vec())
        .unwrap();
    p.drain_binary(Duration::from_millis(50), Duration::from_secs(2));
    assert_eq!(p.close(), b"done$ ");
}
