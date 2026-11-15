//! trzsz 的下载（按顺序预先给出远端 tsz 的输出，不检查客户端的回复，直接运行接收一侧的协议），
//! 以及模拟服务器上 trz 的上传测试。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use md5::{Digest, Md5};

use super::super::info::{Protocol, XferError};
use super::super::pipe::{Pipe, PipeError};
use super::super::transfer::Halt;
use super::super::trzsz::{
    self, Conn, Link, MAX_BLOCK_LEN, MAX_LINE_LEN, decode_bytes, encode_bytes,
};
use super::{PipeOut, entries, noise, quiet_tracker, read_file, wide_pipe, write_file};

fn tsz_line(typ: &str, payload: &str) -> String {
    format!("#{typ}:{payload}\n")
}

fn tsz_string(typ: &str, s: &[u8]) -> String {
    tsz_line(typ, &encode_bytes(s))
}

/// 远端要求覆盖（tsz -y）的配置；`dir` 为真时是传目录（tsz -d）。
fn tsz_config(dir: bool) -> String {
    tsz_string(
        "CFG",
        format!(r#"{{"overwrite":true,"directory":{dir},"timeout":20}}"#).as_bytes(),
    )
}

/// 一个文件的大小、数据与 MD5；`md5_ok` 为假时 MD5 不对。
fn tsz_file(data: &[u8], md5_ok: bool) -> String {
    let sum = if md5_ok {
        Md5::digest(data)
    } else {
        Md5::digest([data, b"!"].concat())
    };
    tsz_line("SIZE", &data.len().to_string()) + &tsz_string("DATA", data) + &tsz_string("MD5", &sum)
}

const NO_CANCEL: &dyn Fn() -> bool = &|| false;

/// 模拟远端 tsz：远端的输出是 `lines`，收到的文件保存到 `dir`；返回结果与客户端的回复。
fn receive_from(dir: &Path, lines: &[String]) -> (Result<(), Halt>, String) {
    let pipe = Pipe::new(16);
    pipe.push_blocking(lines.concat().into_bytes()).unwrap();
    let out = PipeOut::new(wide_pipe());
    let tracker = quiet_tracker(Protocol::Trzsz, false);
    let link = Link {
        pipe: &pipe,
        out: &*out,
        tracker: &tracker,
        windows: false,
        cancelled: NO_CANCEL,
    };
    let result = trzsz::receive(link, dir);
    (result, String::from_utf8_lossy(&out.written()).into_owned())
}

fn failed_with(result: &Result<(), Halt>) -> &XferError {
    match result {
        Err(Halt::Failed(e)) => e,
        other => panic!("应失败：{other:?}"),
    }
}

// 远端要求覆盖（tsz -y）时不理会：触发传输的是普通的终端输出，可以伪造。已存在的同名文件或目录一律改名保存，
// 只写入这次新建的文件与目录；出错时删除的也只有它们，原有的文件与目录不动。
#[test]
fn receive_never_overwrites_existing_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("foo").join("keep.txt"), b"keep");
    let (result, replies) = receive_from(
        dir,
        &[
            tsz_config(false),
            tsz_line("NUM", "1"),
            tsz_string("NAME", b"foo"),
            tsz_file(b"new", true),
        ],
    );
    result.unwrap();
    assert_eq!(read_file(&dir.join("foo").join("keep.txt")), b"keep");
    assert_eq!(read_file(&dir.join("foo (1)")), b"new");
    // 结束语：Saved 1 file to <目录>
    let exit = replies.lines().last().unwrap();
    let message = decode_bytes(exit.trim_start_matches("#EXIT:").as_bytes()).unwrap();
    let message = String::from_utf8(message).unwrap();
    assert!(message.starts_with("Saved 1 file to "), "{message}");
    assert!(message.ends_with("\r\n- foo (1)"), "{message}");
}

/// 固定向量：远端给出写死的 MD5（RFC 1321 的 "abc"），升级 md-5 后照常校验通过。
#[test]
fn receive_checks_fixed_md5() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let md5 = data_encoding::HEXLOWER
        .decode(b"900150983cd24fb0d6963f7d28e17f72")
        .unwrap();
    let (result, _) = receive_from(
        dir,
        &[
            tsz_config(false),
            tsz_line("NUM", "1"),
            tsz_string("NAME", b"abc.txt"),
            tsz_line("SIZE", "3"),
            tsz_string("DATA", b"abc"),
            tsz_string("MD5", &md5),
        ],
    );
    result.unwrap();
    assert_eq!(read_file(&dir.join("abc.txt")), b"abc");
}

#[test]
fn receive_bad_md5_keeps_original() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("a.txt"), b"original");
    let (result, replies) = receive_from(
        dir,
        &[
            tsz_config(false),
            tsz_line("NUM", "1"),
            tsz_string("NAME", b"a.txt"),
            tsz_file(b"xyz", false),
        ],
    );
    assert_eq!(failed_with(&result), &XferError::Checksum, "应校验失败");
    assert_eq!(read_file(&dir.join("a.txt")), b"original");
    assert_eq!(entries(dir), ["a.txt"], "新文件应删除");
    // 告诉远端失败的原因
    assert!(replies.contains("#fail:"), "{replies}");
}

#[test]
fn receive_directory_never_writes_into_existing() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_file(&dir.join(".ssh").join("config"), b"mine");
    let (result, _) = receive_from(
        dir,
        &[
            tsz_config(true),
            tsz_line("NUM", "2"),
            tsz_string(
                "NAME",
                br#"{"path_id":0,"path_name":[".ssh"],"is_dir":true}"#,
            ),
            tsz_string(
                "NAME",
                br#"{"path_id":0,"path_name":[".ssh","config"],"is_dir":false,"size":4}"#,
            ),
            tsz_file(b"evil", true),
        ],
    );
    result.unwrap();
    assert_eq!(read_file(&dir.join(".ssh").join("config")), b"mine");
    assert_eq!(read_file(&dir.join(".ssh (1)").join("config")), b"evil");
}

#[test]
fn receive_directory_failure_removes_only_new_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("tree").join("old.txt"), b"old");
    let (result, _) = receive_from(
        dir,
        &[
            tsz_config(true),
            tsz_line("NUM", "3"),
            tsz_string(
                "NAME",
                br#"{"path_id":0,"path_name":["tree"],"is_dir":true}"#,
            ),
            tsz_string(
                "NAME",
                br#"{"path_id":0,"path_name":["tree","a.txt"],"is_dir":false,"size":1}"#,
            ),
            tsz_file(b"a", true),
            tsz_string(
                "NAME",
                br#"{"path_id":0,"path_name":["tree","b.txt"],"is_dir":false,"size":1}"#,
            ),
            tsz_file(b"b", false),
        ],
    );
    assert!(result.is_err(), "应校验失败");
    assert_eq!(read_file(&dir.join("tree").join("old.txt")), b"old");
    assert_eq!(entries(dir), ["tree"]);
    assert_eq!(entries(&dir.join("tree")), ["old.txt"]);
}

#[test]
fn receive_remote_failure_is_not_answered() {
    // 远端发来 #fail：原样报告，不再回复 #fail
    let tmp = tempfile::tempdir().unwrap();
    let (result, replies) = receive_from(
        tmp.path(),
        &[tsz_config(false), tsz_string("fail", b"No such file: x")],
    );
    assert_eq!(
        failed_with(&result),
        &XferError::Remote("No such file: x".into())
    );
    assert!(!replies.contains("#fail:"), "{replies}");
    // 远端按了 Ctrl+C
    let (result, _) = receive_from(tmp.path(), &[tsz_config(false), "#NU\x03".into()]);
    assert_eq!(failed_with(&result), &XferError::RemoteInterrupted);
}

#[test]
fn receive_windows_lines() {
    // Windows 服务端（或经过 ConPTY）：行以 "!" 结束，夹着终端控制序列与换行
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let windows = |line: String| {
        let line = line.trim_end_matches('\n');
        let (head, tail) = line.split_at(line.len() / 2);
        format!("\x1b[?25l{head}\r\n\x1b[K{tail}!\r\n")
    };
    let lines = [
        windows(tsz_config(false)),
        windows(tsz_line("NUM", "1")),
        windows(tsz_string("NAME", "中文.txt".as_bytes())),
    ];
    let data = b"windows line protocol";
    let sum = Md5::digest(data);
    let more = [
        windows(tsz_line("SIZE", &data.len().to_string())),
        windows(tsz_string("DATA", data)),
        windows(tsz_string("MD5", &sum)),
    ];
    let pipe = Pipe::new(16);
    pipe.push_blocking([lines.concat(), more.concat()].concat().into_bytes())
        .unwrap();
    let out = PipeOut::new(wide_pipe());
    let tracker = quiet_tracker(Protocol::Trzsz, false);
    let link = Link {
        pipe: &pipe,
        out: &*out,
        tracker: &tracker,
        windows: true,
        cancelled: NO_CANCEL,
    };
    trzsz::receive(link, dir).unwrap();
    assert_eq!(read_file(&dir.join("中文.txt")), data);
    // 客户端的行同样以 "!\n" 结束
    let replies = String::from_utf8(out.written()).unwrap();
    assert!(
        replies.starts_with("#ACT:") && replies.contains("!\n#SUCC:1!\n"),
        "{replies}"
    );
    let info = tracker.info();
    assert_eq!(info.files, 1);
    assert_eq!(info.saved, [dir.join("中文.txt")]);
}

// 远端在触发行之后持续输出不换行的数据：一行超过上限时按协议错误中止，不会一直占用内存；错误中提示调小块大小。
#[test]
fn line_limit() {
    for windows in [false, true] {
        let pipe = Arc::new(Pipe::new(16));
        let feeder = {
            let pipe = pipe.clone();
            std::thread::spawn(move || {
                let chunk = vec![b'A'; 1 << 20];
                for _ in 0..(MAX_LINE_LEN >> 20) + 10 {
                    if pipe.push_blocking(chunk.clone()).is_err() {
                        return;
                    }
                }
            })
        };
        let out = PipeOut::new(wide_pipe());
        let tracker = quiet_tracker(Protocol::Trzsz, false);
        let c = Conn::new(Link {
            pipe: &pipe,
            out: &*out,
            tracker: &tracker,
            windows,
            cancelled: NO_CANCEL,
        });
        let result = c.recv("DATA", false);
        let _ = pipe.close();
        feeder.join().unwrap();
        match result {
            Err(Halt::Failed(e @ XferError::LineTooLong)) => {
                let text = e.to_string();
                assert!(text.contains("上限") && text.contains("-B"), "{text}");
            }
            other => panic!("windows={windows}：应按协议错误中止并提示调小块大小：{other:?}"),
        }
    }
}

// 远端用 tsz -B 设了较大的块：在解码上限（64MB）以内的块都能接收。
#[test]
fn large_block() {
    let tmp = tempfile::tempdir().unwrap();
    let data = noise(30 << 20, 3);
    let (result, _) = receive_from(
        tmp.path(),
        &[
            tsz_config(false),
            tsz_line("NUM", "1"),
            tsz_string("NAME", b"big.bin"),
            tsz_file(&data, true),
        ],
    );
    result.unwrap();
    assert!(
        read_file(&tmp.path().join("big.bin")) == data,
        "接收的内容不一致"
    );
}

// 一个数据块解压后超过上限（例如压缩率很高的数据）：报错并提示调小块大小，不截断后继续。
#[test]
fn block_limit() {
    assert_eq!(
        decode_bytes(encode_bytes(&vec![0u8; MAX_BLOCK_LEN]).as_bytes())
            .unwrap()
            .len(),
        MAX_BLOCK_LEN,
        "上限以内的块应能解码"
    );
    let e = decode_bytes(encode_bytes(&vec![0u8; MAX_BLOCK_LEN + 1]).as_bytes()).unwrap_err();
    assert_eq!(e, XferError::BlockTooLarge);
    assert!(e.to_string().contains("-B"), "{e}");
}

// #CFG：超时为 0 或负数（远端关掉超时）时用默认值，过大时用上限；数据块大小同样有上限。
#[test]
fn config_limits() {
    for (timeout, want) in [(0i64, 20i64), (-5, 20), (60, 60), (1 << 30, 300)] {
        let cfg = format!(r#"{{"timeout":{timeout},"bufsize":{}}}"#, 1i64 << 40);
        let pipe = Pipe::new(16);
        pipe.push_blocking(tsz_string("CFG", cfg.as_bytes()).into_bytes())
            .unwrap();
        let out = PipeOut::new(wide_pipe());
        let tracker = quiet_tracker(Protocol::Trzsz, false);
        let mut c = Conn::new(Link {
            pipe: &pipe,
            out: &*out,
            tracker: &tracker,
            windows: false,
            cancelled: NO_CANCEL,
        });
        c.recv_config().unwrap();
        assert_eq!(c.cfg.timeout, want, "timeout={timeout}");
        assert_eq!(c.cfg.max_buf_size, 10 << 20);
    }
    // 不支持的功能
    for (cfg, what) in [
        (r#"{"binary":true}"#, "二进制模式"),
        (r#"{"protocol":2}"#, "新版协议"),
    ] {
        let pipe = Pipe::new(16);
        pipe.push_blocking(tsz_string("CFG", cfg.as_bytes()).into_bytes())
            .unwrap();
        let out = PipeOut::new(wide_pipe());
        let tracker = quiet_tracker(Protocol::Trzsz, false);
        let mut c = Conn::new(Link {
            pipe: &pipe,
            out: &*out,
            tracker: &tracker,
            windows: false,
            cancelled: NO_CANCEL,
        });
        match c.recv_config() {
            Err(Halt::Failed(XferError::UnsupportedTrzsz(w))) => assert_eq!(w, what),
            other => panic!("{cfg}：{other:?}"),
        }
    }
}

// #CFG 的换行只接受 "\n" 与 "!\n"：别的值会被拼在我方每一行后面，等于让输出往 shell 里写命令。
#[test]
fn config_rejects_injected_newline() {
    for (newline, ok) in [
        ("\\n", true),
        ("!\\n", true),
        ("\\ncurl x|sh\\n", false),
        ("\\r\\n", false),
        ("", false),
    ] {
        let cfg = format!(r#"{{"newline":"{newline}"}}"#);
        let pipe = Pipe::new(16);
        pipe.push_blocking(tsz_string("CFG", cfg.as_bytes()).into_bytes())
            .unwrap();
        let out = PipeOut::new(wide_pipe());
        let tracker = quiet_tracker(Protocol::Trzsz, false);
        let mut c = Conn::new(Link {
            pipe: &pipe,
            out: &*out,
            tracker: &tracker,
            windows: false,
            cancelled: NO_CANCEL,
        });
        match c.recv_config() {
            Ok(()) => {
                assert!(ok, "{cfg} 应被拒绝");
                assert!(c.cfg.newline == "\n" || c.cfg.newline == "!\n");
            }
            Err(Halt::Failed(XferError::Protocol { .. })) => {
                assert!(!ok, "{cfg} 应被接受");
                assert_eq!(c.cfg.newline, "\n", "拒绝后仍用自己的换行");
            }
            Err(other) => panic!("{cfg}：{other:?}"),
        }
    }
}

// ------------------------------------------------------------ 模拟服务器上的 trz

/// 读一行（到 "\n" 为止，不含）。
fn read_line(pipe: &Pipe) -> String {
    let mut line = Vec::new();
    loop {
        let chunk = pipe
            .next(Some(
                std::time::Instant::now() + std::time::Duration::from_secs(10),
            ))
            .unwrap_or_else(|e: PipeError| panic!("等客户端的行：{e:?}，已收到 {line:?}"));
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            line.extend_from_slice(&chunk[..i]);
            pipe.unread(&chunk[i + 1..]);
            return String::from_utf8(line).unwrap();
        }
        line.extend_from_slice(&chunk);
    }
}

fn split_line(line: &str) -> (&str, &str) {
    let line = line
        .strip_prefix('#')
        .unwrap_or_else(|| panic!("不是协议的行：{line}"));
    line.split_once(':').unwrap()
}

/// 服务器上收到的一项：名称（目录模式下是相对路径）与内容（目录为 `None`）。
type Received = Vec<(String, Option<Vec<u8>>)>;

/// 模拟服务器上的 trz：从 `input` 读客户端的行，经 `reply` 回复；返回收到的文件。
fn fake_trz(input: &Pipe, reply: &PipeOut, cfg: &str) -> Received {
    let send = |typ: &str, payload: &str| {
        reply
            .pipe
            .push_blocking(format!("#{typ}:{payload}\n").into_bytes())
            .unwrap();
    };
    let (typ, act) = split_line_owned(read_line(input));
    assert_eq!(typ, "ACT");
    let act: serde_json::Value =
        serde_json::from_slice(&decode_bytes(act.as_bytes()).unwrap()).unwrap();
    assert_eq!(act["confirm"], true);
    send("CFG", &encode_bytes(cfg.as_bytes()));
    let mut received = Received::new();
    loop {
        let (typ, payload) = split_line_owned(read_line(input));
        match typ.as_str() {
            "NUM" => send("SUCC", &payload),
            "NAME" => {
                let name = String::from_utf8(decode_bytes(payload.as_bytes()).unwrap()).unwrap();
                let (shown, is_dir) = match serde_json::from_str::<serde_json::Value>(&name) {
                    Ok(desc) if desc.is_object() => {
                        let parts: Vec<String> = desc["path_name"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|p| p.as_str().unwrap().to_string())
                            .collect();
                        (parts.join("/"), desc["is_dir"].as_bool().unwrap())
                    }
                    _ => (name.clone(), false),
                };
                let top = shown.split('/').next().unwrap().to_string();
                send("SUCC", &encode_bytes(top.as_bytes()));
                if is_dir {
                    received.push((shown, None));
                    continue;
                }
                let (typ, size) = split_line_owned(read_line(input));
                assert_eq!(typ, "SIZE");
                send("SUCC", &size);
                let size: usize = size.parse().unwrap();
                let mut data = Vec::new();
                while data.len() < size {
                    let (typ, payload) = split_line_owned(read_line(input));
                    assert_eq!(typ, "DATA");
                    let block = decode_bytes(payload.as_bytes()).unwrap();
                    send("SUCC", &block.len().to_string());
                    data.extend(block);
                }
                let (typ, md5) = split_line_owned(read_line(input));
                assert_eq!(typ, "MD5");
                assert_eq!(
                    decode_bytes(md5.as_bytes()).unwrap(),
                    Md5::digest(&data)[..]
                );
                send("SUCC", &md5);
                received.push((shown, Some(data)));
            }
            "EXIT" => return received,
            "fail" => {
                let message = decode_bytes(payload.as_bytes()).unwrap();
                received.push((format!("#fail {}", String::from_utf8_lossy(&message)), None));
                return received;
            }
            other => panic!("意外的行：{other}"),
        }
    }
}

fn split_line_owned(line: String) -> (String, String) {
    let (typ, payload) = split_line(&line);
    (typ.to_string(), payload.to_string())
}

/// 经模拟的 trz 上传 `paths`；返回结果、服务器收到的内容与进度记录。
fn upload(
    paths: &[PathBuf],
    directory: bool,
    cfg: &str,
) -> (Result<(), Halt>, Received, super::super::info::TransferInfo) {
    // 客户端读 `from_server`，写进 `to_server`
    let from_server = Arc::new(Pipe::new(1 << 20));
    let to_server = wide_pipe();
    let reply = PipeOut::new(from_server.clone());
    let cfg = cfg.to_string();
    let server = {
        let to_server = to_server.clone();
        std::thread::spawn(move || fake_trz(&to_server, &reply, &cfg))
    };
    let out = PipeOut::new(to_server);
    let tracker = quiet_tracker(Protocol::Trzsz, true);
    let link = Link {
        pipe: &from_server,
        out: &*out,
        tracker: &tracker,
        windows: false,
        cancelled: NO_CANCEL,
    };
    let result = trzsz::send(link, paths, directory);
    (result, server.join().unwrap(), tracker.info())
}

#[test]
fn trz_uploads_files() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a.bin");
    let b = tmp.path().join("中文.txt");
    let big = noise(3 << 20, 9);
    write_file(&a, &big);
    write_file(&b, "你好".as_bytes());
    let (result, received, info) = upload(&[a, b], false, r#"{"timeout":20}"#);
    result.unwrap();
    assert_eq!(received.len(), 2);
    assert_eq!(received[0].0, "a.bin");
    assert!(received[0].1.as_deref() == Some(&big[..]));
    assert_eq!(
        received[1],
        ("中文.txt".into(), Some("你好".as_bytes().to_vec()))
    );
    assert_eq!(info.files, 2);
    assert_eq!(info.total_files, 2);
    assert_eq!(info.bytes, info.total_bytes);
    assert_eq!(info.uploaded, ["a.bin", "中文.txt"]);
}

#[test]
fn trz_d_uploads_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("localtree");
    write_file(&tree.join("deep").join("c.txt"), b"c");
    std::fs::create_dir_all(tree.join("empty")).unwrap();
    let (result, received, info) = upload(&[tree], true, r#"{"directory":true}"#);
    result.unwrap();
    let names: Vec<&str> = received.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "localtree",
            "localtree/deep",
            "localtree/deep/c.txt",
            "localtree/empty"
        ]
    );
    assert_eq!(received[2].1.as_deref(), Some(&b"c"[..]));
    assert_eq!(
        info.uploaded.last().map(String::as_str),
        Some("localtree/empty")
    );
}

#[test]
fn trz_rejects_directory_without_d_and_duplicate_names() {
    let tmp = tempfile::tempdir().unwrap();
    let tree = tmp.path().join("tree");
    write_file(&tree.join("x.txt"), b"x");
    // 选了目录但服务器上不是 trz -d：不确认，提示用 trz -d
    let pipe = Pipe::new(16);
    let out = PipeOut::new(wide_pipe());
    let tracker = quiet_tracker(Protocol::Trzsz, true);
    let link = Link {
        pipe: &pipe,
        out: &*out,
        tracker: &tracker,
        windows: false,
        cancelled: NO_CANCEL,
    };
    match trzsz::send(link, std::slice::from_ref(&tree), false) {
        Err(Halt::Failed(XferError::TrzNeedsDirectory(p))) => assert!(p.ends_with("tree")),
        other => panic!("{other:?}"),
    }
    let act = String::from_utf8(out.written()).unwrap();
    let act = decode_bytes(act.trim().trim_start_matches("#ACT:").as_bytes()).unwrap();
    assert!(
        String::from_utf8(act)
            .unwrap()
            .contains(r#""confirm":false"#)
    );

    // 服务器要求覆盖（trz -y）时，所选的文件中有重名的：报错并告诉服务器
    let other = tmp.path().join("other");
    write_file(&other.join("x.txt"), b"y");
    let (result, received, _) = upload(
        &[tree.join("x.txt"), other.join("x.txt")],
        false,
        r#"{"overwrite":true}"#,
    );
    assert_eq!(
        failed_with(&result),
        &XferError::DuplicateName("x.txt".into())
    );
    assert!(
        received.last().unwrap().0.starts_with("#fail "),
        "{received:?}"
    );
}

#[test]
fn trzsz_errors_mention_protocol() {
    let e = XferError::protocol(Protocol::Trzsz, "应为 NUM，收到 X");
    assert_eq!(e.to_string(), "trzsz 协议错误：应为 NUM，收到 X");
}
