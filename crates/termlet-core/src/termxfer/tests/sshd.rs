//! 集成测试：经 SSH 在测试容器（testing/sshd，带 lrzsz 与 trzsz-go 的 trz / tsz）中运行
//! sz / rz / tsz / trz，终端的字节流经 [`Interceptor`]，验证与真实程序的互通。需要 Docker 中的测试服务器：
//!
//! ```text
//! TERMLET_TEST_SSH=127.0.0.1:2222 cargo test -p termlet-core termxfer
//! ```
//!
//! 同一容器中的 busybox telnetd（`TERMLET_TEST_TELNET=127.0.0.1:2323`）上测 Telnet 中的 ZModem。

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use termlet_io::{GridSize, IoEvent, TerminalIo};

use super::super::info::{Answer, Protocol, TransferInfo, TransferState, XferError};
use super::super::{InterceptOptions, Interceptor, LinkInfo};
use super::{entries, read_file, write_file};
use crate::rawterm::{RawConfig, RawSpec, RawTarget};
use crate::secret::SecretString;
use crate::ssh::{ConnectOptions, Connection, ShellOptions, exec, open_shell};
use crate::telnet::TelnetPlan;
use crate::testkit::{
    SSH_PASSWORD, SSH_USER, connect_test, memory_store, test_server, test_telnet,
};

/// 对每个等待回答的传输给出的回答；`None` 表示拒绝。
type Answerer = Box<dyn Fn(&TransferInfo) -> Option<Answer> + Send>;

struct Harness {
    /// SSH 连接（另开通道准备、检查远端的文件）；Telnet 时没有
    connection: Option<Arc<Connection>>,
    interceptor: Interceptor,
    events: async_channel::Receiver<TransferInfo>,
    term: TerminalIo,
    text: Mutex<String>,
    /// 上一次传输没有成功结束时，这之前不会识别 ZModem（见 Detector::suppress_zmodem）：下一步等过这段时间
    quiet_until: Mutex<Option<Instant>>,
}

impl Harness {
    fn new(command: Option<String>, link: LinkInfo) -> Option<Self> {
        let (address, port) = test_server()?;
        let rt = crate::runtime::runtime();
        let connection = rt.block_on(connect_test(&address, port));
        let io = rt
            .block_on(open_shell(
                connection.clone(),
                ShellOptions {
                    size: GridSize::new(120, 40),
                    command,
                    ..Default::default()
                },
            ))
            .unwrap();
        let interceptor = Interceptor::new(InterceptOptions {
            answer_timeout: Duration::from_secs(30),
            progress_every: Duration::from_millis(20),
            ..InterceptOptions::default()
        });
        let events = interceptor.events();
        let term = interceptor.wrap(io, link);
        Some(Self {
            connection: Some(connection),
            interceptor,
            events,
            term,
            text: Mutex::default(),
            quiet_until: Mutex::default(),
        })
    }

    /// 经 Telnet 登录容器（自动登录），终端的字节流经 [`Interceptor`]。
    fn telnet() -> Option<Self> {
        let (address, port) = test_telnet()?;
        let password = || Some(SecretString::new(SSH_PASSWORD));
        let spec = RawSpec {
            target: RawTarget::Telnet(TelnetPlan {
                label: address.clone(),
                address,
                port,
                username: SSH_USER.into(),
                password: password(),
                proxy: None,
                jumps: Vec::new(),
                term_type: "xterm-256color".into(),
            }),
            config: RawConfig {
                eof_exit: true,
                username: SSH_USER.into(),
                password: password(),
                ..Default::default()
            },
        };
        let raw = crate::runtime::runtime()
            .block_on(crate::rawterm::open(
                memory_store(),
                spec,
                GridSize::new(120, 40),
                crate::ssh::no_prompt(),
                &ConnectOptions::default(),
            ))
            .unwrap();
        let interceptor = Interceptor::new(InterceptOptions {
            answer_timeout: Duration::from_secs(30),
            progress_every: Duration::from_millis(20),
            ..InterceptOptions::default()
        });
        let events = interceptor.events();
        let term = interceptor.wrap(raw.io, LinkInfo::default());
        let h = Self {
            connection: None,
            interceptor,
            events,
            term,
            text: Mutex::default(),
            quiet_until: Mutex::default(),
        };
        // 等自动登录完成
        let deadline = Instant::now() + Duration::from_secs(20);
        while !h.text().contains("$ ") {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "Telnet 登录超时：{:?}", h.text());
            let event = crate::runtime::runtime()
                .block_on(async { tokio::time::timeout(left, h.term.output.recv()).await });
            if let Ok(Ok(IoEvent::Output(bytes))) = event {
                h.text
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes));
            }
        }
        Some(h)
    }

    /// 在另一个通道中执行命令（准备、检查远端的文件）。
    fn exec(&self, command: &str) -> String {
        let connection = self.connection.as_ref().expect("只有 SSH 能另开通道");
        let result = crate::runtime::runtime()
            .block_on(exec(connection, command, 1 << 20, Duration::from_secs(60)))
            .unwrap();
        assert_eq!(
            result.exit_code,
            Some(0),
            "{command}：{}",
            String::from_utf8_lossy(&result.stderr)
        );
        String::from_utf8_lossy(&result.stdout).into_owned()
    }

    fn remote_md5(&self, path: &str) -> String {
        self.exec(&format!("md5sum '{path}'"))
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }

    /// 在终端中执行命令，直到出现 `marker`（且开始了的传输已结束）；返回这期间的传输状态。
    /// `on_running` 在第一次收到进度时调用（测试中途取消）。
    fn run(
        &self,
        command: &str,
        marker: &str,
        answer: &Answerer,
        on_running: Option<&dyn Fn(&Interceptor, &TransferInfo)>,
    ) -> Vec<TransferInfo> {
        self.text.lock().unwrap().clear();
        if let Some(until) = self.quiet_until.lock().unwrap().take() {
            std::thread::sleep(until.saturating_duration_since(Instant::now()));
        }
        // 命令行的回显中也有标记：用算式，只在命令执行完后才输出 <标记>-42
        self.term
            .input
            .write(format!("{command}; echo {marker}-$((40+2))\n").as_bytes());
        let want = format!("{marker}-42");
        let mut infos: Vec<TransferInfo> = Vec::new();
        let mut ran = false;
        let deadline = Instant::now() + Duration::from_secs(120);
        let rt = crate::runtime::runtime();
        loop {
            let finished = infos.last().is_none_or(|i| i.state.is_finished());
            if finished && self.text.lock().unwrap().contains(&want) {
                return infos;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let text = self.text.lock().unwrap();
                let tail: String = text
                    .chars()
                    .rev()
                    .take(600)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect();
                panic!(
                    "等待 {want} 超时；状态 {:?}；终端输出的结尾：{tail:?}",
                    infos.last()
                );
            }
            enum Next {
                Output(IoEvent),
                Info(Box<TransferInfo>),
                Timeout,
            }
            let next = rt.block_on(async {
                tokio::select! {
                    event = self.term.output.recv() => match event {
                        Ok(event) => Next::Output(event),
                        Err(_) => panic!("SSH 会话已断开"),
                    },
                    info = self.events.recv() => Next::Info(Box::new(info.unwrap())),
                    _ = tokio::time::sleep(left) => Next::Timeout,
                }
            });
            match next {
                Next::Output(IoEvent::Output(bytes)) => self
                    .text
                    .lock()
                    .unwrap()
                    .push_str(&String::from_utf8_lossy(&bytes)),
                Next::Output(IoEvent::Closed(reason)) => panic!("SSH 会话已结束：{reason:?}"),
                Next::Info(info) => {
                    let info = *info;
                    match info.state {
                        TransferState::Waiting => {
                            let reply = answer(&info).unwrap_or(Answer::Refuse);
                            self.interceptor.answer(info.id, reply).unwrap();
                        }
                        TransferState::Running if info.file_bytes > 0 && !ran => {
                            if let Some(f) = on_running {
                                ran = true;
                                f(&self.interceptor, &info);
                            }
                        }
                        TransferState::Failed(_) | TransferState::Cancelled => {
                            *self.quiet_until.lock().unwrap() =
                                Some(Instant::now() + Duration::from_millis(2200));
                        }
                        _ => {}
                    }
                    infos.push(info);
                }
                Next::Timeout => {}
            }
        }
    }

    fn text(&self) -> String {
        self.text.lock().unwrap().clone()
    }
}

fn last(infos: &[TransferInfo]) -> TransferInfo {
    infos.last().cloned().expect("没有开始传输")
}

fn md5_file(path: &Path) -> String {
    data_encoding::HEXLOWER.encode(&Md5::digest(read_file(path)))
}

fn save_to(dir: &Path, protocol: Protocol) -> Answerer {
    let dir = dir.to_path_buf();
    Box::new(move |info| {
        assert!(
            !info.upload && info.protocol == protocol,
            "应为 {protocol:?} 下载：{info:?}"
        );
        Some(Answer::SaveTo(dir.clone()))
    })
}

fn send(paths: Vec<std::path::PathBuf>) -> Answerer {
    Box::new(move |_| Some(Answer::Send(paths.clone())))
}

#[test]
fn ssh_transfers() {
    let Some(h) = Harness::new(None, LinkInfo::default()) else {
        return;
    };
    let remote = format!("/tmp/termxfer-{}", crate::id::random_token(6));
    h.exec(&format!(
        "mkdir -p {remote}/up {remote}/tree/sub && cd {remote} && head -c 3000000 /dev/urandom > big.bin && \
         printf 'hello 中文' > '中文.txt' && echo a > tree/a.txt && echo b > tree/sub/b.txt"
    ));
    let big_md5 = h.remote_md5(&format!("{remote}/big.bin"));
    h.term.input.write(format!("cd {remote}\n").as_bytes());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        steps(&h, &remote, &big_md5)
    }));
    h.exec(&format!("rm -rf {remote}"));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn steps(h: &Harness, remote: &str, big_md5: &str) {
    // sz 下载（含中文文件名，本机已有同名文件时改名）
    let dir = tempfile::tempdir().unwrap();
    write_file(&dir.path().join("big.bin"), b"old");
    let info = last(&h.run(
        "sz big.bin 中文.txt",
        "SZ-DONE",
        &save_to(dir.path(), Protocol::Zmodem),
        None,
    ));
    assert_eq!(info.state, TransferState::Done, "{info:?}");
    assert_eq!(info.files, 2);
    assert_eq!(info.dir.as_deref(), Some(dir.path()));
    assert_eq!(
        md5_file(&dir.path().join("big (1).bin")),
        big_md5,
        "big.bin 内容不对"
    );
    assert_eq!(
        read_file(&dir.path().join("中文.txt")),
        "hello 中文".as_bytes()
    );
    assert!(info.speed > 0 || info.bytes > 0);

    // rz 上传；远端已有同名文件时跳过
    let local_dir = tempfile::tempdir().unwrap();
    let local = local_dir.path().join("up.bin");
    write_file(&local, &b"termlet".repeat(200_000));
    let info = last(&h.run(
        "cd up && rz; cd ..",
        "RZ-DONE",
        &send(vec![local.clone()]),
        None,
    ));
    assert_eq!(info.state, TransferState::Done, "{info:?}");
    assert_eq!(info.files, 1);
    assert_eq!(
        h.remote_md5(&format!("{remote}/up/up.bin")),
        md5_file(&local)
    );
    let info = last(&h.run(
        "cd up && rz; cd ..",
        "RZ-AGAIN",
        &send(vec![local.clone()]),
        None,
    ));
    assert_eq!(
        info.skipped,
        ["up.bin"],
        "远端已有同名文件时应跳过：{info:?}"
    );
    assert_eq!(info.files, 0);
    // rz -y 覆盖
    let info = last(&h.run(
        "cd up && rz -y; cd ..",
        "RZ-Y",
        &send(vec![local.clone()]),
        None,
    ));
    assert_eq!(
        (info.state.clone(), info.files),
        (TransferState::Done, 1),
        "{info:?}"
    );

    // 拒绝与中途取消：远端程序退出，删除写了一半的文件
    let refuse: Answerer = Box::new(|_| None);
    let info = last(&h.run("sz big.bin", "SZ-REFUSED", &refuse, None));
    assert_eq!(info.state, TransferState::Cancelled, "{info:?}");
    let dir = tempfile::tempdir().unwrap();
    let cancel = |x: &Interceptor, info: &TransferInfo| {
        x.cancel(info.id);
    };
    let info = last(&h.run(
        "cat big.bin big.bin big.bin > big3.bin && sz big3.bin",
        "SZ-CANCELLED",
        &save_to(dir.path(), Protocol::Zmodem),
        Some(&cancel),
    ));
    assert_eq!(info.state, TransferState::Cancelled, "{info:?}");
    assert!(entries(dir.path()).is_empty(), "应删除写了一半的文件");

    // 刚回答就取消：sz 退出后的输出（下一条命令、提示符）照常显示
    let dir = tempfile::tempdir().unwrap();
    h.exec(&format!("head -c 60000000 /dev/zero > {remote}/zero.bin"));
    let quick = {
        let dir = dir.path().to_path_buf();
        let interceptor = h.interceptor.clone();
        Box::new(move |info: &TransferInfo| {
            let interceptor = interceptor.clone();
            let id = info.id;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                interceptor.cancel(id);
            });
            Some(Answer::SaveTo(dir.clone()))
        }) as Answerer
    };
    let info = last(&h.run("sz zero.bin", "SZ-EARLY", &quick, None));
    assert_eq!(info.state, TransferState::Cancelled, "{info:?}");
    assert!(entries(dir.path()).is_empty(), "应删除写了一半的文件");

    // tsz 下载
    let dir = tempfile::tempdir().unwrap();
    let info = last(&h.run(
        "tsz big.bin 中文.txt",
        "TSZ-DONE",
        &save_to(dir.path(), Protocol::Trzsz),
        None,
    ));
    assert_eq!(info.state, TransferState::Done, "{info:?} {:?}", h.text());
    assert_eq!(info.files, 2);
    assert_eq!(
        md5_file(&dir.path().join("big.bin")),
        big_md5,
        "big.bin 内容不对"
    );
    assert!(
        h.text().contains("Saved 2 files"),
        "远端应显示结束语：{:?}",
        h.text()
    );

    // trz 上传与 trz -d 上传目录、tsz -d 下载目录
    let local = local_dir.path().join("t.bin");
    write_file(&local, &b"trzsz".repeat(300_000));
    let info = last(&h.run(
        "cd up && trz; cd ..",
        "TRZ-DONE",
        &send(vec![local.clone()]),
        None,
    ));
    assert_eq!(info.state, TransferState::Done, "{info:?} {:?}", h.text());
    assert_eq!(
        h.remote_md5(&format!("{remote}/up/t.bin")),
        md5_file(&local)
    );
    let tree = local_dir.path().join("localtree");
    write_file(&tree.join("deep").join("c.txt"), b"c");
    let dirs: Answerer = {
        let tree = tree.clone();
        Box::new(move |info| {
            assert!(info.directory, "应为上传目录：{info:?}");
            Some(Answer::Send(vec![tree.clone()]))
        })
    };
    let info = last(&h.run("cd up && trz -d; cd ..", "TRZD-DONE", &dirs, None));
    assert_eq!(info.state, TransferState::Done, "{info:?} {:?}", h.text());
    assert_eq!(
        h.exec(&format!("cat {remote}/up/localtree/deep/c.txt"))
            .trim(),
        "c"
    );
    let down = tempfile::tempdir().unwrap();
    let info = last(&h.run(
        "tsz -d tree",
        "TSZD-DONE",
        &save_to(down.path(), Protocol::Trzsz),
        None,
    ));
    assert_eq!(info.state, TransferState::Done, "{info:?} {:?}", h.text());
    assert_eq!(
        read_file(&down.path().join("tree").join("sub").join("b.txt")),
        b"b\n"
    );

    // tsz 中途取消：远端显示原因并退出，删除写了一半的文件
    let dir = tempfile::tempdir().unwrap();
    let info = last(&h.run(
        "tsz big3.bin",
        "TSZ-CANCELLED",
        &save_to(dir.path(), Protocol::Trzsz),
        Some(&cancel),
    ));
    assert_eq!(info.state, TransferState::Cancelled, "{info:?}");
    assert!(
        h.text().contains("Stopped"),
        "远端应显示 Stopped：{:?}",
        h.text()
    );
    assert!(entries(dir.path()).is_empty(), "应删除写了一半的文件");

    // trz 拒绝；选了目录但不是 trz -d 时提示
    let info = last(&h.run("trz", "TRZ-REFUSED", &refuse, None));
    assert_eq!(info.state, TransferState::Cancelled, "{info:?}");
    assert!(h.text().contains("Cancelled"), "{:?}", h.text());
    let a_dir = tempfile::tempdir().unwrap();
    let info = last(&h.run(
        "trz",
        "TRZ-DIR",
        &send(vec![a_dir.path().to_path_buf()]),
        None,
    ));
    assert!(
        matches!(
            info.state,
            TransferState::Failed(XferError::TrzNeedsDirectory(_))
        ),
        "应提示用 trz -d：{info:?}"
    );
}

#[test]
fn transfers_in_tmux() {
    // 主机开启了 tmux 续连：trzsz 在 tmux 中照常可用
    let name = format!("termlet-xfer-{}", crate::id::random_token(6).to_lowercase());
    let Some(h) = Harness::new(
        Some(format!("exec tmux new-session -A -s {name}")),
        LinkInfo {
            conpty: false,
            tmux: true,
        },
    ) else {
        return;
    };
    let remote = format!("/tmp/termxfer-tmux-{}", crate::id::random_token(6));
    h.exec(&format!(
        "mkdir -p {remote} && head -c 500000 /dev/urandom > {remote}/r.bin"
    ));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        h.term.input.write(format!("cd {remote}\n").as_bytes());
        let dir = tempfile::tempdir().unwrap();
        let info = last(&h.run(
            "tsz r.bin",
            "TMUX-TSZ",
            &save_to(dir.path(), Protocol::Trzsz),
            None,
        ));
        assert_eq!(info.state, TransferState::Done, "{info:?} {:?}", h.text());
        assert_eq!(
            md5_file(&dir.path().join("r.bin")),
            h.remote_md5(&format!("{remote}/r.bin"))
        );
        let local = dir.path().join("back.bin");
        write_file(&local, &b"tmux".repeat(100_000));
        let info = last(&h.run("trz", "TMUX-TRZ", &send(vec![local.clone()]), None));
        assert_eq!(info.state, TransferState::Done, "{info:?} {:?}", h.text());
        assert_eq!(
            h.remote_md5(&format!("{remote}/back.bin")),
            md5_file(&local)
        );
        // ZModem 的帧头经过 tmux 后已不完整（tmux 不转发其中的控制字符），不会开始传输：
        // 不在这里测 sz（它要等很久才超时退出），ZModem 在 tmux 中说明原因见 interceptor 的测试
    }));
    let _ = crate::runtime::runtime().block_on(exec(
        h.connection.as_ref().unwrap(),
        &format!("rm -rf {remote}; tmux kill-session -t {name}"),
        1024,
        Duration::from_secs(10),
    ));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

// trz、tsz 在 tmux 中先输出 `\n\x1b[1A\x1b[0J`（清掉光标以下），再输出触发行。两次输出被 tmux 一次读到时（远端调度慢，
// CI 上偶发），tmux 用滚动区域清屏，这一批的最后复位滚动区域、用 CUP 定位光标：客户端收到的触发行后面没有 \r\n，
// 紧跟着 `\x1b[1;40r\x1b[10;1H`。以前的检测要等到换行才认，trz 一直等不到回答（transfers_in_tmux 偶发超时）。
// 这里用 printf 一次写出两段（与 tmux 一次读到相同），每次都是这样的输出。
#[test]
fn trigger_redrawn_by_tmux() {
    let name = format!("termlet-xfer-{}", crate::id::random_token(6).to_lowercase());
    let Some(h) = Harness::new(
        Some(format!("exec tmux new-session -A -s {name}")),
        LinkInfo {
            conpty: false,
            tmux: true,
        },
    ) else {
        return;
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 命令行的回显中不能出现完整的触发行：魔串拆开，由 printf 拼出；没有识别出来时 read 20 秒后放弃
        let command = r"m=TRZSZ; printf '\n\033[1A\033[0J\033[s::%s:TRANSFER:R:1.2.0:%s:0\r\n' $m 1234567890120; read -t 20 -r reply";
        let refuse: Answerer = Box::new(|_| None);
        let infos = h.run(command, "TMUX-TRIGGER", &refuse, None);
        let text = h.text();
        let first = infos
            .first()
            .unwrap_or_else(|| panic!("没有识别出触发行：{text:?}"));
        assert_eq!(
            (first.protocol, first.upload, &first.state),
            (Protocol::Trzsz, true, &TransferState::Waiting),
            "{first:?}"
        );
        assert_eq!(last(&infos).state, TransferState::Cancelled);
        // 第一次显示触发行时就识别了（显示的 TRZSZ 改为 TRZSZGO）；没有识别时要等 tmux 过几秒整屏重绘、
        // 触发行后面有了 \r\n 才认出来，或者一直等不到
        let at = text
            .find("TRANSFER:R:1.2.0:1234567890120")
            .expect("应显示触发行");
        assert!(
            text[..at].ends_with("::TRZSZGO:"),
            "第一次显示触发行时应识别：{text:?}"
        );
    }));
    let _ = crate::runtime::runtime().block_on(exec(
        h.connection.as_ref().unwrap(),
        &format!("tmux kill-session -t {name}"),
        1024,
        Duration::from_secs(10),
    ));
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// 在终端中执行 md5sum，取出 `name` 的校验和（Telnet 不能另开通道）。
fn md5_in_terminal(h: &Harness, name: &str, marker: &str) -> String {
    let refuse: Answerer = Box::new(|_| None);
    h.run(&format!("md5sum {name}"), marker, &refuse, None);
    let text = h.text();
    let pattern =
        regex::Regex::new(&format!(r"([0-9a-f]{{32}})  {}", regex::escape(name))).unwrap();
    pattern
        .captures(&text)
        .unwrap_or_else(|| panic!("没有 {name} 的校验和：{text:?}"))[1]
        .to_string()
}

// Telnet 没有协商二进制模式：telnetd 按网络虚拟终端的规则改写回车与 NUL。ZModem 双方都转义全部控制字符后
// 下载、上传含任意字节的文件都不出错（busybox telnetd 与 lrzsz）。
#[test]
fn zmodem_over_telnet() {
    let Some(h) = Harness::telnet() else {
        println!("未设置 TERMLET_TEST_TELNET，跳过");
        return;
    };
    let remote = format!("/tmp/termxfer-telnet-{}", crate::id::random_token(6));
    let refuse: Answerer = Box::new(|_| None);
    h.run(
        &format!("mkdir -p {remote}/up && cd {remote} && head -c 300000 /dev/urandom > big.bin"),
        "PREPARED",
        &refuse,
        None,
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let big_md5 = md5_in_terminal(&h, "big.bin", "BIG-MD5");
        let dir = tempfile::tempdir().unwrap();
        let info = last(&h.run(
            "sz big.bin",
            "TELNET-SZ",
            &save_to(dir.path(), Protocol::Zmodem),
            None,
        ));
        assert_eq!(info.state, TransferState::Done, "{info:?}");
        assert_eq!(
            md5_file(&dir.path().join("big.bin")),
            big_md5,
            "下载的内容不对"
        );

        let local = dir.path().join("up.bin");
        write_file(&local, &super::noise(300_000, 23));
        let info = last(&h.run(
            "cd up && rz; cd ..",
            "TELNET-RZ",
            &send(vec![local.clone()]),
            None,
        ));
        assert_eq!(info.state, TransferState::Done, "{info:?}");
        assert_eq!(
            md5_in_terminal(&h, "up/up.bin", "UP-MD5"),
            md5_file(&local),
            "上传的内容不对"
        );
    }));
    h.run(
        &format!("cd / && rm -rf {remote}"),
        "CLEANED",
        &refuse,
        None,
    );
    h.term.input.write(b"exit\n");
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
