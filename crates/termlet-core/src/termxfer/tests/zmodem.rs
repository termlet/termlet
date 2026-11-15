//! ZModem 的回环测试：本模块的发送与接收互相收发（经两个管道模拟线路），远端一侧用内存中的文件。

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use termlet_io::{GridSize, TerminalInput};

use super::super::info::{Protocol, XferError};
use super::super::pipe::Pipe;
use super::super::transfer::Halt;
use super::super::zmodem::files::{self as zfiles, Link};
use super::super::zmodem::receiver::{Outcome, ReceiveFiles};
use super::super::zmodem::sender::SendFiles;
use super::super::zmodem::{
    Config, FileInfo, Header, Offer, Session, ZError, ZRINIT, crc16, marshal_file_info,
    parse_file_info, widen,
};
use super::{PipeOut, entries, noise, quiet_tracker, read_file, wide_pipe, write_file};

#[test]
fn file_info_round_trip() {
    let mtime = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let offer = Offer {
        name: r"dir\中文 a.txt".into(),
        size: 12345,
        mtime: Some(mtime),
        mode: 0o100644,
        files_left: 3,
        bytes_left: 99999,
    };
    let data = marshal_file_info(&offer);
    assert!(data.starts_with("dir/中文 a.txt\0".as_bytes()));
    let info = parse_file_info(&data).unwrap();
    assert_eq!(info.name, "dir/中文 a.txt".as_bytes());
    assert_eq!(info.size, 12345);
    assert_eq!(info.mtime, Some(mtime));
    assert_eq!(info.mode, 0o100644);
    assert_eq!((info.files_left, info.bytes_left), (3, 99999));
    // lrzsz 的写法；名称之后的各项都可以没有
    let info = parse_file_info(b"a.txt\x0010 14567044521 100600 0 1 10\0").unwrap();
    assert_eq!((info.size, info.mode, info.files_left), (10, 0o100600, 1));
    let info = parse_file_info(b"only\0").unwrap();
    assert_eq!((info.size, info.mtime), (0, None));
    assert!(parse_file_info(b"no terminator").is_err());
}

#[test]
fn headers_and_positions() {
    let h = Header::pos(ZRINIT, 0x1234_5678);
    assert_eq!(h.data, [0x78, 0x56, 0x34, 0x12], "位置按小端");
    assert_eq!(h.position(), 0x1234_5678);
    assert_eq!(h.zf0(), 0x12, "标志 ZF0 在最后一个字节");
    // 32 位的位置还原成离当前位置最近的完整位置（超过 4 GB 的文件）
    assert_eq!(widen(10, 5), 10);
    assert_eq!(widen(0xffff_fff0, 0x1_0000_0010), 0xffff_fff0);
    assert_eq!(widen(0x10, 0x1_ffff_fff0), 0x2_0000_0010);
    assert_eq!(crc16(b"123456789"), 0x31c3, "CRC-16/XMODEM 的标准校验值");
}

/// 内存中的文件：远端一侧的 sz（发送）或 rz（接收）。
#[derive(Default)]
pub(super) struct Remote {
    pub files: Vec<(String, Vec<u8>)>,
    pub next: usize,
    pub pos: usize,
    /// 每读一块等这么久（让传输持续一段时间）
    pub slow: Option<Duration>,
    pub received: Vec<(Vec<u8>, Vec<u8>)>,
    pub current: Vec<u8>,
    /// 接收时不要的文件（模拟服务器上已有同名文件、rz 没加 -y）
    pub refuse: HashSet<String>,
    pub outcomes: Vec<Outcome>,
    /// 发出第一个文件之前删掉这个目录（模拟保存目录不能写入）
    pub remove_dir: Option<std::path::PathBuf>,
}

impl SendFiles for Remote {
    fn next_file(&mut self) -> Result<Option<Offer>, ZError> {
        if let Some(dir) = self.remove_dir.take() {
            std::fs::remove_dir_all(dir).unwrap();
        }
        let Some((name, data)) = self.files.get(self.next) else {
            return Ok(None);
        };
        self.next += 1;
        self.pos = 0;
        Ok(Some(Offer {
            name: name.clone(),
            size: data.len() as u64,
            mtime: Some(UNIX_EPOCH + Duration::from_secs(1_600_000_000)),
            mode: 0o100644,
            files_left: (self.files.len() - self.next + 1) as u32,
            bytes_left: 0,
        }))
    }

    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ZError> {
        if let Some(slow) = self.slow {
            std::thread::sleep(slow);
        }
        let data = &self.files[self.next - 1].1;
        let n = buf.len().min(data.len() - self.pos);
        buf[..n].copy_from_slice(&data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }

    fn seek(&mut self, pos: u64) -> Result<(), ZError> {
        self.pos = pos as usize;
        Ok(())
    }

    fn crc32(&mut self, _len: u64) -> Result<u32, ZError> {
        Ok(0)
    }

    fn progress(&mut self, _offer: &Offer, _sent: u64) {}

    fn completed(&mut self, _offer: &Offer, _sent: u64, outcome: Outcome) {
        self.outcomes.push(outcome);
    }
}

impl ReceiveFiles for Remote {
    fn accept(&mut self, info: &FileInfo) -> bool {
        self.current.clear();
        !self.refuse.contains(&*String::from_utf8_lossy(&info.name))
    }

    fn write(&mut self, data: &[u8]) -> Result<(), ZError> {
        self.current.extend_from_slice(data);
        Ok(())
    }

    fn progress(&mut self, _info: &FileInfo, _received: u64) {}

    fn completed(&mut self, info: &FileInfo, _received: u64, outcome: Outcome) {
        self.outcomes.push(outcome);
        if outcome == Outcome::Done {
            self.received
                .push((info.name.clone(), std::mem::take(&mut self.current)));
        }
    }
}

/// 一条线路：两个方向各一个管道。`local_in` 是本机读的（远端写的），`remote_in` 是远端读的。
struct Line {
    local_in: Arc<Pipe>,
    remote_in: Arc<Pipe>,
    local_out: Arc<PipeOut>,
    remote_out: Arc<PipeOut>,
}

fn line() -> Line {
    let local_in = wide_pipe();
    let remote_in = wide_pipe();
    Line {
        local_out: PipeOut::new(remote_in.clone()),
        remote_out: PipeOut::new(local_in.clone()),
        local_in,
        remote_in,
    }
}

/// 在另一个线程上运行远端的会话（发送或接收）。
pub(super) fn run_remote(
    pipe: Arc<Pipe>,
    out: Arc<dyn TerminalInput>,
    mut remote: Remote,
    send: bool,
    cfg: Config,
) -> std::thread::JoinHandle<(Result<(), ZError>, Remote)> {
    std::thread::spawn(move || {
        let mut session = Session::new(&pipe, &*out, cfg);
        let result = if send {
            session.send(&mut remote)
        } else {
            session.receive(&mut remote)
        };
        session.finish();
        (result, remote)
    })
}

#[test]
fn download_from_remote_sender() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    write_file(&dir.join("big.bin"), b"old");
    let big = noise(1_500_000, 1);
    let all_bytes: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    let remote = Remote {
        files: vec![
            ("big.bin".into(), big.clone()),
            ("中文.txt".into(), "hello 中文".as_bytes().to_vec()),
            ("empty".into(), Vec::new()),
            ("bytes.bin".into(), all_bytes.clone()),
        ],
        ..Remote::default()
    };
    let l = line();
    let peer = run_remote(
        l.remote_in.clone(),
        l.remote_out.clone(),
        remote,
        true,
        Config::default(),
    );
    let tracker = quiet_tracker(Protocol::Zmodem, false);
    let link = Link {
        pipe: &l.local_in,
        out: &*l.local_out,
        tracker: &tracker,
        escape_ctl: false,
    };
    zfiles::receive(&link, dir).unwrap();
    let (result, remote) = peer.join().unwrap();
    result.unwrap();
    assert_eq!(remote.outcomes, [Outcome::Done; 4]);
    // 本机已有同名文件时改名，已有的不动
    assert_eq!(read_file(&dir.join("big.bin")), b"old");
    assert!(read_file(&dir.join("big (1).bin")) == big);
    assert_eq!(read_file(&dir.join("中文.txt")), "hello 中文".as_bytes());
    assert_eq!(read_file(&dir.join("empty")), b"");
    assert!(read_file(&dir.join("bytes.bin")) == all_bytes);
    // 修改时间按对方给的
    let mtime = std::fs::metadata(dir.join("中文.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_600_000_000));
    let info = tracker.info();
    assert_eq!(info.files, 4);
    assert_eq!(info.total_files, 4, "按对方给的剩余文件数");
    assert_eq!(info.bytes, (big.len() + 12 + all_bytes.len()) as u64);
    assert_eq!(info.saved[0], dir.join("big (1).bin"));
    // 远端退出后的输出（"OO" 之后的）留给终端
    let leftover = l.local_in.close();
    assert!(leftover.is_empty(), "{leftover:?}");
}

#[test]
fn upload_to_remote_receiver_with_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("up.bin");
    let b = tmp.path().join("exists.txt");
    let data = b"termlet".repeat(200_000);
    write_file(&a, &data);
    write_file(&b, b"x");
    let remote = Remote {
        refuse: ["exists.txt".to_string()].into(),
        ..Remote::default()
    };
    let l = line();
    let peer = run_remote(
        l.remote_in.clone(),
        l.remote_out.clone(),
        remote,
        false,
        Config::default(),
    );
    let tracker = quiet_tracker(Protocol::Zmodem, true);
    let link = Link {
        pipe: &l.local_in,
        out: &*l.local_out,
        tracker: &tracker,
        escape_ctl: false,
    };
    zfiles::send(&link, &[a, b]).unwrap();
    let (result, remote) = peer.join().unwrap();
    result.unwrap();
    assert_eq!(remote.received.len(), 1);
    assert_eq!(remote.received[0].0, b"up.bin");
    assert!(remote.received[0].1 == data);
    let info = tracker.info();
    assert_eq!(info.uploaded, ["up.bin"]);
    assert_eq!(info.skipped, ["exists.txt"], "远端已有同名文件时跳过");
    assert_eq!((info.files, info.total_files), (1, 2));
}

#[test]
fn zmodem_cannot_upload_directories() {
    let tmp = tempfile::tempdir().unwrap();
    let l = line();
    let tracker = quiet_tracker(Protocol::Zmodem, true);
    let link = Link {
        pipe: &l.local_in,
        out: &*l.local_out,
        tracker: &tracker,
        escape_ctl: false,
    };
    match zfiles::send(&link, &[tmp.path().to_path_buf()]) {
        Err(Halt::Failed(XferError::ZmodemDirectory(_))) => {}
        other => panic!("{other:?}"),
    }
    // 告诉远端的 rz 不传了（中止序列）
    assert!(l.local_out.written().starts_with(&[0x18; 10]));
}

#[test]
fn crc16_session() {
    // 一方不支持 CRC-32 时用 CRC-16
    let cfg = Config {
        use_crc32: false,
        ..Config::default()
    };
    let data = noise(300_000, 5);
    let remote = Remote {
        files: vec![("a".into(), data.clone())],
        ..Remote::default()
    };
    let l = line();
    let peer = run_remote(
        l.remote_in.clone(),
        l.remote_out.clone(),
        remote,
        true,
        cfg.clone(),
    );
    let mut local = Remote::default();
    let mut session = Session::new(&l.local_in, &*l.local_out, cfg);
    session.receive(&mut local).unwrap();
    session.finish();
    peer.join().unwrap().0.unwrap();
    assert!(local.received[0].1 == data);
}

/// 线路上在第 `at` 个字节处改坏一个字节（只一次）；同时插进 XON / XOFF（应被忽略）。
struct Noisy {
    inner: Arc<PipeOut>,
    at: usize,
    seen: AtomicUsize,
    done: Mutex<bool>,
}

impl TerminalInput for Noisy {
    fn write(&self, data: &[u8]) {
        let start = self.seen.fetch_add(data.len(), Ordering::SeqCst);
        let mut data = data.to_vec();
        let mut done = self.done.lock().unwrap();
        if !*done && start <= self.at && self.at < start + data.len() {
            data[self.at - start] ^= 0x55;
            data.insert(0, 0x11);
            *done = true;
        }
        self.inner.write(&data);
    }

    fn resize(&self, _size: GridSize) {}
}

#[test]
fn recovers_from_corrupted_data() {
    // 数据子包损坏：接收方请发送方从写到的位置重发，文件仍然完整
    let data = noise(400_000, 11);
    let remote = Remote {
        files: vec![("x.bin".into(), data.clone())],
        ..Remote::default()
    };
    let l = line();
    let noisy: Arc<dyn TerminalInput> = Arc::new(Noisy {
        inner: l.remote_out.clone(),
        at: 150_000,
        seen: AtomicUsize::new(0),
        done: Mutex::new(false),
    });
    let peer = run_remote(l.remote_in.clone(), noisy, remote, true, Config::default());
    let mut local = Remote::default();
    let mut session = Session::new(&l.local_in, &*l.local_out, Config::default());
    session.receive(&mut local).unwrap();
    session.finish();
    peer.join().unwrap().0.unwrap();
    assert_eq!(local.received.len(), 1);
    assert!(local.received[0].1 == data, "重发后内容应完整");
}

#[test]
fn cancel_removes_partial_and_aborts_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().to_path_buf();
    let remote = Remote {
        files: vec![("slow.bin".into(), noise(4 << 20, 2))],
        slow: Some(Duration::from_millis(2)),
        ..Remote::default()
    };
    let l = line();
    let peer = run_remote(
        l.remote_in.clone(),
        l.remote_out.clone(),
        remote,
        true,
        Config::default(),
    );
    let local_in = l.local_in.clone();
    let canceller = {
        let dir = dir.clone();
        let pipe = local_in.clone();
        std::thread::spawn(move || {
            // 等写了一部分再取消
            for _ in 0..500 {
                std::thread::sleep(Duration::from_millis(10));
                if std::fs::read_dir(&dir).unwrap().next().is_some() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            pipe.cancel();
        })
    };
    let tracker = quiet_tracker(Protocol::Zmodem, false);
    let link = Link {
        pipe: &l.local_in,
        out: &*l.local_out,
        tracker: &tracker,
        escape_ctl: false,
    };
    let result = zfiles::receive(&link, &dir);
    canceller.join().unwrap();
    assert!(matches!(result, Err(Halt::Cancelled)), "{result:?}");
    assert!(
        entries(&dir).is_empty(),
        "应删除写了一半的文件：{:?}",
        entries(&dir)
    );
    // 远端收到中止序列后退出
    let (remote_result, _) = peer.join().unwrap();
    assert!(
        matches!(remote_result, Err(ZError::Aborted)),
        "{remote_result:?}"
    );
    // 取消后只发了中止序列（协议的写入丢弃）
    let written = l.local_out.written();
    assert!(
        written.ends_with(&super::super::zmodem::ABORT),
        "取消后最后发出的是中止序列"
    );
}

#[test]
fn bad_save_dir() {
    let l = line();
    let tracker = quiet_tracker(Protocol::Zmodem, false);
    let link = Link {
        pipe: &l.local_in,
        out: &*l.local_out,
        tracker: &tracker,
        escape_ctl: false,
    };
    let missing = std::env::temp_dir().join(format!(
        "termxfer-missing-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    match zfiles::receive(&link, &missing) {
        Err(Halt::Failed(XferError::BadSaveDir(p))) => assert_eq!(p, missing),
        other => panic!("{other:?}"),
    }
}

// 保存目录不能写入（这里是被删掉了）：不能静默地把每个文件记成“跳过”、整次显示完成，要报告原因
#[test]
fn receive_reports_why_files_cannot_be_created() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("save");
    std::fs::create_dir(&dir).unwrap();
    let remote = Remote {
        files: vec![
            ("a.txt".into(), b"a".to_vec()),
            ("b.txt".into(), b"b".to_vec()),
        ],
        remove_dir: Some(dir.clone()),
        ..Remote::default()
    };
    let l = line();
    let peer = run_remote(
        l.remote_in.clone(),
        l.remote_out.clone(),
        remote,
        true,
        Config::default(),
    );
    let tracker = quiet_tracker(Protocol::Zmodem, false);
    let link = Link {
        pipe: &l.local_in,
        out: &*l.local_out,
        tracker: &tracker,
        escape_ctl: false,
    };
    match zfiles::receive(&link, &dir) {
        Err(Halt::Failed(XferError::Local { path, .. })) => {
            assert_eq!(path.file_name().unwrap(), "a.txt")
        }
        other => panic!("{other:?}"),
    }
    let _ = peer.join().unwrap();
    assert!(tracker.info().skipped.is_empty(), "不算跳过");
}
