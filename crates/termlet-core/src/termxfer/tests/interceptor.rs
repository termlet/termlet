//! 经 [`Interceptor`] 的完整流程：远端用本模块的协议实现模拟 sz / rz（或按顺序给出 tsz 的输出），
//! 终端一侧读包装后的输出、写包装后的输入。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use termlet_io::{CloseReason, GridSize, IoEvent, TerminalInput, TerminalIo};

use super::super::info::{Answer, Protocol, TransferInfo, TransferState, XferError};
use super::super::pipe::Pipe;
use super::super::trzsz::encode_bytes;
use super::super::zmodem::{Config, ZError};
use super::super::{InterceptOptions, Interceptor, LinkInfo};
use super::zmodem::{Remote, run_remote};
use super::{entries, noise, read_file, wide_pipe, write_file};

/// 终端一侧写给远端的输入：记下写过的内容与 set_binary 的调用，再交给远端。
struct FakeInput {
    remote_in: Arc<Pipe>,
    written: Mutex<Vec<u8>>,
    binary: Mutex<Vec<bool>>,
    closed: AtomicBool,
}

impl TerminalInput for FakeInput {
    fn write(&self, data: &[u8]) {
        self.written.lock().unwrap().extend_from_slice(data);
        let _ = self.remote_in.push_blocking(data.to_vec());
    }

    fn resize(&self, _size: GridSize) {}

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn set_binary(&self, on: bool) {
        self.binary.lock().unwrap().push(on);
    }
}

/// 远端的输出：送给终端一侧。
struct RemoteOut(async_channel::Sender<IoEvent>);

impl TerminalInput for RemoteOut {
    fn write(&self, data: &[u8]) {
        let _ = self.0.send_blocking(IoEvent::Output(data.to_vec()));
    }

    fn resize(&self, _size: GridSize) {}
}

struct Harness {
    interceptor: Interceptor,
    events: async_channel::Receiver<TransferInfo>,
    term: TerminalIo,
    input: Arc<FakeInput>,
    remote_in: Arc<Pipe>,
    remote_out: Arc<RemoteOut>,
    text: Mutex<Vec<u8>>,
}

fn harness(link: LinkInfo, options: InterceptOptions) -> Harness {
    let (tx, rx) = async_channel::bounded(64);
    let remote_in = wide_pipe();
    let input = Arc::new(FakeInput {
        remote_in: remote_in.clone(),
        written: Mutex::default(),
        binary: Mutex::default(),
        closed: AtomicBool::new(false),
    });
    let interceptor = Interceptor::new(options);
    let events = interceptor.events();
    let term = interceptor.wrap(
        TerminalIo {
            output: rx,
            input: input.clone(),
        },
        link,
    );
    Harness {
        interceptor,
        events,
        term,
        input,
        remote_in,
        remote_out: Arc::new(RemoteOut(tx)),
        text: Mutex::default(),
    }
}

fn default_harness() -> Harness {
    harness(LinkInfo::default(), InterceptOptions::default())
}

impl Harness {
    /// 读终端一侧的输出，直到出现 `needle`；返回目前为止的全部输出。
    fn text_until(&self, needle: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            {
                let text = self.text.lock().unwrap();
                let s = String::from_utf8_lossy(&text);
                if s.contains(needle) {
                    return s.into_owned();
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(
                !left.is_zero(),
                "等待 {needle:?} 超时：{:?}",
                String::from_utf8_lossy(&self.text.lock().unwrap())
            );
            let event = crate::runtime::runtime()
                .block_on(async { tokio::time::timeout(left, self.term.output.recv()).await });
            match event {
                Ok(Ok(IoEvent::Output(bytes))) => self.text.lock().unwrap().extend(bytes),
                Ok(Ok(IoEvent::Closed(_))) | Ok(Err(_)) => {
                    panic!("终端已结束，没有等到 {needle:?}")
                }
                Err(_) => {}
            }
        }
    }

    /// 等第一个满足条件的状态。
    fn event(&self, what: &str, pred: impl Fn(&TransferInfo) -> bool) -> TransferInfo {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = crate::runtime::runtime()
                .block_on(async { tokio::time::timeout(left, self.events.recv()).await });
            match event {
                Ok(Ok(info)) if pred(&info) => return info,
                Ok(Ok(_)) => {}
                _ => panic!("等待{what}超时"),
            }
        }
    }

    fn waiting(&self) -> TransferInfo {
        self.event("等待回答", |i| i.state == TransferState::Waiting)
    }

    fn finished(&self) -> TransferInfo {
        self.event("传输结束", |i| i.state.is_finished())
    }

    fn written(&self) -> Vec<u8> {
        self.input.written.lock().unwrap().clone()
    }

    fn say(&self, text: &str) {
        self.remote_out.write(text.as_bytes());
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn sz_download() {
    let h = default_harness();
    let tmp = tempfile::tempdir().unwrap();
    let big = noise(2_000_000, 21);
    h.say("$ sz big.bin 中文.txt\r\n");
    let remote = run_remote(
        h.remote_in.clone(),
        h.remote_out.clone(),
        Remote {
            files: vec![
                ("big.bin".into(), big.clone()),
                ("中文.txt".into(), "hello 中文".as_bytes().to_vec()),
            ],
            ..Remote::default()
        },
        true,
        Config::default(),
    );
    let info = h.waiting();
    assert_eq!(info.protocol, Protocol::Zmodem);
    assert!(!info.upload);
    assert_eq!(h.interceptor.accepts_drop(), None, "下载时不接收拖来的文件");
    // 等待回答期间用户的输入不发给远端
    h.term.input.write(b"typed-early");
    assert_eq!(
        h.interceptor
            .answer(info.id, Answer::Send(vec![PathBuf::from("x")])),
        Err(XferError::WrongAnswer)
    );
    h.interceptor
        .answer(info.id, Answer::SaveTo(tmp.path().to_path_buf()))
        .unwrap();
    assert_eq!(
        h.interceptor
            .answer(info.id, Answer::SaveTo(tmp.path().to_path_buf())),
        Err(XferError::NotWaiting),
        "只能回答一次"
    );
    let running = h.event("开始传输", |i| i.state == TransferState::Running);
    assert_eq!(running.dir.as_deref(), Some(tmp.path()));
    let done = h.finished();
    assert_eq!(done.state, TransferState::Done, "{done:?}");
    assert_eq!(done.files, 2);
    assert_eq!(
        done.saved,
        [tmp.path().join("big.bin"), tmp.path().join("中文.txt")]
    );
    let (result, _) = remote.join().unwrap();
    result.unwrap();
    assert!(read_file(&tmp.path().join("big.bin")) == big);
    assert_eq!(
        read_file(&tmp.path().join("中文.txt")),
        "hello 中文".as_bytes()
    );
    // 远端程序退出后的输出照常显示，传输的数据不显示
    h.say("\r\nSZ-DONE-42\r\n$ ");
    let text = h.text_until("SZ-DONE-42");
    assert!(text.starts_with("$ sz big.bin 中文.txt\r\n"), "{text:?}");
    assert!(!text.contains("**\x18B"), "帧头不应显示：{text:?}");
    assert!(text.len() < 200, "传输的数据不应显示：{} 字节", text.len());
    // 期间数据原样收发；结束后输入照常
    assert_eq!(*h.input.binary.lock().unwrap(), [true, false]);
    assert!(!contains(&h.written(), b"typed-early"));
    h.term.input.write(b"ls\r");
    assert!(h.written().ends_with(b"ls\r"));
    assert_eq!(h.interceptor.current().unwrap().state, TransferState::Done);
}

#[test]
fn rz_upload_by_drop_with_skip() {
    let h = default_harness();
    let tmp = tempfile::tempdir().unwrap();
    let up = tmp.path().join("up.bin");
    let exists = tmp.path().join("exists.txt");
    let data = b"termlet".repeat(150_000);
    write_file(&up, &data);
    write_file(&exists, b"x");
    h.say("rz waiting to receive.");
    let remote = run_remote(
        h.remote_in.clone(),
        h.remote_out.clone(),
        Remote {
            refuse: ["exists.txt".to_string()].into(),
            ..Remote::default()
        },
        false,
        Config::default(),
    );
    let info = h.waiting();
    assert!(info.upload && !info.directory);
    // 把文件拖到终端上作答
    let id = h
        .interceptor
        .accepts_drop()
        .expect("等待上传时接收拖来的文件");
    assert_eq!(id, info.id);
    h.interceptor
        .answer(id, Answer::Send(vec![up, exists]))
        .unwrap();
    let done = h.finished();
    assert_eq!(done.state, TransferState::Done, "{done:?}");
    assert_eq!(done.uploaded, ["up.bin"]);
    assert_eq!(done.skipped, ["exists.txt"], "远端已有同名文件时跳过并报告");
    let (result, remote) = remote.join().unwrap();
    result.unwrap();
    assert!(remote.received[0].1 == data);
    assert!(
        h.text_until("rz waiting")
            .starts_with("rz waiting to receive.")
    );
}

#[test]
fn refuse_aborts_remote() {
    let h = default_harness();
    let remote = run_remote(
        h.remote_in.clone(),
        h.remote_out.clone(),
        Remote {
            files: vec![("a".into(), vec![1; 1000])],
            ..Remote::default()
        },
        true,
        Config::default(),
    );
    let info = h.waiting();
    h.interceptor.answer(info.id, Answer::Refuse).unwrap();
    assert_eq!(h.finished().state, TransferState::Cancelled);
    let (result, _) = remote.join().unwrap();
    assert!(matches!(result, Err(ZError::Aborted)), "{result:?}");
    // 远端退出后 shell 的提示符照常显示
    h.say("$ after-refuse");
    assert!(h.text_until("after-refuse").ends_with("$ after-refuse"));
}

#[test]
fn ctrl_c_cancels_and_removes_partial() {
    let h = default_harness();
    let tmp = tempfile::tempdir().unwrap();
    let remote = run_remote(
        h.remote_in.clone(),
        h.remote_out.clone(),
        Remote {
            files: vec![("slow.bin".into(), noise(6 << 20, 4))],
            slow: Some(Duration::from_millis(2)),
            ..Remote::default()
        },
        true,
        Config::default(),
    );
    let info = h.waiting();
    h.interceptor
        .answer(info.id, Answer::SaveTo(tmp.path().to_path_buf()))
        .unwrap();
    h.event("传输了一部分", |i| i.file_bytes > 0);
    // 在终端中按 Ctrl+C
    h.term.input.write(b"\x03");
    let done = h.finished();
    assert_eq!(done.state, TransferState::Cancelled);
    assert!(entries(tmp.path()).is_empty(), "应删除写了一半的文件");
    let (result, _) = remote.join().unwrap();
    assert!(matches!(result, Err(ZError::Aborted)), "{result:?}");
    assert!(!h.interceptor.cancel(info.id), "已结束的不能再取消");
}

#[test]
fn zmodem_refused_over_conpty_and_tmux() {
    for (link, want) in [
        (
            LinkInfo {
                conpty: true,
                tmux: false,
            },
            XferError::ZmodemOverConpty,
        ),
        (
            LinkInfo {
                conpty: false,
                tmux: true,
            },
            XferError::ZmodemInTmux,
        ),
    ] {
        let h = harness(link, InterceptOptions::default());
        let remote = run_remote(
            h.remote_in.clone(),
            h.remote_out.clone(),
            Remote {
                files: vec![("a".into(), vec![1; 10])],
                ..Remote::default()
            },
            true,
            Config::default(),
        );
        let done = h.finished();
        assert_eq!(done.state, TransferState::Failed(want));
        let (result, _) = remote.join().unwrap();
        assert!(matches!(result, Err(ZError::Aborted)), "{result:?}");
    }
}

#[test]
fn disabled_passes_output_through() {
    let h = default_harness();
    h.interceptor.set_enabled(false);
    assert!(!h.interceptor.is_enabled());
    h.remote_out.write(b"x**\x18B00000000000000\r\x8a\x11 end");
    let text = h.text_until(" end");
    assert!(text.contains("**\x18B000000"), "{text:?}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(h.events.try_recv().is_err(), "关闭识别后不应开始传输");
    h.term.input.write(b"typed");
    assert!(h.written().ends_with(b"typed"));
}

#[test]
fn held_output_is_flushed() {
    // 末尾可能是起始序列的开头时暂扣，等不到后续时照常显示
    let h = default_harness();
    h.say("abc*");
    assert_eq!(h.text_until("abc"), "abc");
    assert_eq!(h.text_until("*"), "abc*");
}

#[test]
fn trigger_at_the_end_starts_after_hold() {
    // 远端输出触发行后就等回答：触发行后面一直没有别的输出（没有换行）时，暂扣到期后照常开始传输
    let h = default_harness();
    h.say("$ trz\r\n::TRZSZ:TRANSFER:R:1.2.0:1728631234567220:0");
    let info = h.waiting();
    assert!(info.upload && info.protocol == Protocol::Trzsz, "{info:?}");
    let text = h.text_until("TRZSZGO");
    assert!(
        text.ends_with("::TRZSZGO:TRANSFER:R:1.2.0:1728631234567220:0"),
        "{text:?}"
    );
    assert!(h.interceptor.cancel(info.id));
    assert_eq!(h.finished().state, TransferState::Cancelled);
}

#[test]
fn tsz_download_and_answer_timeout() {
    let h = default_harness();
    let tmp = tempfile::tempdir().unwrap();
    let data = noise(200_000, 8);
    let line = |typ: &str, payload: &str| format!("#{typ}:{payload}\n");
    let string = |typ: &str, s: &[u8]| line(typ, &encode_bytes(s));
    h.say("$ tsz a.bin\r\n\x1b7\x07::TRZSZ:TRANSFER:S:1.2.0:1728631234567800\r\n");
    h.say(
        &[
            string("CFG", br#"{"timeout":20}"#),
            line("NUM", "1"),
            string("NAME", b"a.bin"),
            line("SIZE", &data.len().to_string()),
            string("DATA", &data),
            string("MD5", &Md5::digest(&data)),
        ]
        .concat(),
    );
    let info = h.waiting();
    assert_eq!(info.protocol, Protocol::Trzsz);
    h.interceptor
        .answer(info.id, Answer::SaveTo(tmp.path().to_path_buf()))
        .unwrap();
    let done = h.finished();
    assert_eq!(done.state, TransferState::Done, "{done:?}");
    assert!(read_file(&tmp.path().join("a.bin")) == data);
    let replies = String::from_utf8_lossy(&h.written()).into_owned();
    assert!(
        replies.starts_with("#ACT:") && replies.contains("#EXIT:"),
        "{replies}"
    );
    let text = h.text_until("TRZSZGO");
    assert!(text.contains("::TRZSZGO:TRANSFER:S"), "{text:?}");

    // 没有回答：超时后视为取消，告诉远端不传了
    let h = harness(
        LinkInfo::default(),
        InterceptOptions {
            answer_timeout: Duration::from_millis(200),
            ..InterceptOptions::default()
        },
    );
    h.say("::TRZSZ:TRANSFER:R:1.2.0:1728631234567900\r\n");
    h.waiting();
    assert_eq!(h.finished().state, TransferState::Cancelled);
    let replies = String::from_utf8_lossy(&h.written()).into_owned();
    let act =
        super::super::trzsz::decode_bytes(replies.trim().trim_start_matches("#ACT:").as_bytes())
            .unwrap();
    assert!(
        String::from_utf8(act)
            .unwrap()
            .contains(r#""confirm":false"#)
    );
}

// 等待回答期间远端持续输出（误判或伪造的起始序列，后面是 cat 大日志）：照常读出，不会因为没人读而让远端停住；
// 超过上限后按“不是文件传输”结束，不回复远端，存下的输出按原来的顺序显示。
#[test]
fn flood_while_waiting_is_not_a_transfer() {
    let h = default_harness();
    h.say("::TRZSZ:TRANSFER:S:1.2.0:1728631234567811\r\n");
    h.waiting();
    let out = h.remote_out.clone();
    let line = "x".repeat(1023) + "\n";
    let flood = std::thread::spawn(move || {
        for i in 0..1600 {
            out.write(format!("{i:05}{line}").as_bytes());
        }
        out.write(b"$ end-of-flood");
    });
    let done = h.finished();
    assert!(
        matches!(
            done.state,
            TransferState::Failed(XferError::Protocol { .. })
        ),
        "{done:?}"
    );
    let text = h.text_until("end-of-flood");
    flood.join().unwrap();
    assert!(h.written().is_empty(), "不应回复远端");
    // 没有丢、没有乱序
    let mut expect = 0;
    for l in text.lines().filter(|l| l.len() == 1028) {
        assert_eq!(l[..5].parse::<usize>().unwrap(), expect);
        expect += 1;
    }
    assert_eq!(expect, 1600);
}

#[test]
fn disconnect_fails_transfer_and_forwards_close() {
    let h = default_harness();
    let tmp = tempfile::tempdir().unwrap();
    let _remote = run_remote(
        h.remote_in.clone(),
        h.remote_out.clone(),
        Remote {
            files: vec![("slow.bin".into(), noise(6 << 20, 6))],
            slow: Some(Duration::from_millis(2)),
            ..Remote::default()
        },
        true,
        Config::default(),
    );
    let info = h.waiting();
    h.interceptor
        .answer(info.id, Answer::SaveTo(tmp.path().to_path_buf()))
        .unwrap();
    h.event("传输了一部分", |i| i.file_bytes > 0);
    h.remote_out
        .0
        .send_blocking(IoEvent::Closed(CloseReason::Lost("断开".into())))
        .unwrap();
    let done = h.finished();
    assert_eq!(done.state, TransferState::Failed(XferError::Disconnected));
    assert!(entries(tmp.path()).is_empty());
    // 结束通知照常交给终端
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = crate::runtime::runtime()
            .block_on(async { tokio::time::timeout(left, h.term.output.recv()).await })
            .expect("等待结束通知超时");
        if let Ok(IoEvent::Closed(reason)) = event {
            assert_eq!(reason, CloseReason::Lost("断开".into()));
            break;
        }
    }
    // 关闭终端：交给原来的传输
    h.term.input.close();
    assert!(h.input.closed.load(Ordering::SeqCst));
}
