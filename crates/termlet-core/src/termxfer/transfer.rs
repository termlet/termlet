//! 一次传输：等待用户回答、在自己的线程上运行协议、记录结果。
//!
//! 远端的输出经 [`Pipe`] 交给它，它直接写远端的输入（原始的 [`TerminalInput`]，不经过拦截）。
//! 结果先于 [`Transfer::done_receiver`] 上报：上报之后（[`Transfer::finished`]）用户的输入就可以照常发给远端，
//! 转发任务在 done 之后才把传输摘下、取回协议没读走的输出。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use termlet_io::TerminalInput;
use tokio::sync::watch;

use super::detect::Trigger;
use super::info::{Answer, Protocol, TransferId, TransferInfo, TransferState, XferError};
use super::pipe::{PIPE_CAPACITY, Pipe};
use super::progress::{Report, Tracker};
use super::trzsz;
use super::zmodem::files as zfiles;

/// 等待回答期间最多替协议存下多少远端输出：照常读出，远端不会因为没人读而停住整条连接（SSH 的同一连接上还有
/// 别的终端、SFTP、转发）。真正的 sz / rz / trzsz 在等待期间只发很短的起始帧，超过时不是文件传输（误判或伪造的起始序列）。
const WAIT_BUFFER_LIMIT: usize = 1 << 20;
/// 等待回答时多久读一次远端输出。
const WAIT_POLL: Duration = Duration::from_millis(20);

/// 等待回答的结果。
enum Waited {
    Answer(Answer),
    /// 取消、超时、拒绝或选择为空
    NoAnswer,
    /// 期间远端输出太多：不是文件传输
    Overflow,
}

/// 传输为什么停下来。
#[derive(Debug)]
pub(crate) enum Halt {
    /// 用户取消、拒绝、没有回答
    Cancelled,
    Failed(XferError),
}

/// 传输的参数。
#[derive(Clone, Debug)]
pub(crate) struct TransferOptions {
    /// 等待用户选择文件或保存位置的最长时间，超时视为取消
    pub answer_timeout: Duration,
    /// 上报进度的最短间隔
    pub progress_every: Duration,
    /// 会话经过 Windows 的伪终端（本地终端、WSL）：ConPTY 会改写输出，ZModem 的二进制数据无法通过，
    /// trzsz 改用 Windows 的行协议（与 trzsz-go 对 Windows 服务端的处理相同）
    pub conpty: bool,
    /// 终端在 tmux 中：tmux 不转发 ZModem 的数据（tmux 的维护者明确不支持），trzsz 可以
    pub tmux: bool,
    /// 线路会改写控制字符（非二进制模式的 Telnet）：ZModem 双方都转义全部控制字符
    pub escape_ctl: bool,
}

enum Slot {
    /// 还在等
    Pending,
    Given(Answer),
    /// 已经不再接受回答
    Closed,
}

pub(crate) struct Transfer {
    pub id: TransferId,
    pub trigger: Trigger,
    options: TransferOptions,
    pub pipe: Pipe,
    out: Arc<dyn TerminalInput>,
    tracker: Tracker,
    answer: Mutex<Slot>,
    answered: Condvar,
    cancelled: AtomicBool,
    /// 终端断开（不是用户取消）：结果记为失败
    disconnected: AtomicBool,
    finished: AtomicBool,
    done: watch::Sender<bool>,
}

impl Transfer {
    /// 创建一次传输，`rest` 是起始序列之后已收到的输出。之后调用 [`Transfer::begin`] 开始。
    pub(crate) fn new(
        trigger: Trigger,
        rest: Vec<u8>,
        out: Arc<dyn TerminalInput>,
        options: TransferOptions,
        report: Report,
    ) -> Arc<Self> {
        let id = TransferId::next();
        let info = TransferInfo::new(id, trigger.protocol, trigger.upload, trigger.directory);
        let pipe = Pipe::new(PIPE_CAPACITY);
        // 新的管道是空的，放得下
        let _ = pipe.push_blocking(rest);
        Arc::new(Self {
            id,
            tracker: Tracker::new(info, options.progress_every, report),
            trigger,
            options,
            pipe,
            out,
            answer: Mutex::new(Slot::Pending),
            answered: Condvar::new(),
            cancelled: AtomicBool::new(false),
            disconnected: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            done: watch::channel(false).0,
        })
    }

    /// 在自己的线程上开始：先请用户选择（上报等待回答），之后运行协议。
    pub(crate) fn begin(self: &Arc<Self>) {
        let me = self.clone();
        let spawned = std::thread::Builder::new()
            .name("termxfer".into())
            .spawn(move || me.run());
        if let Err(e) = spawned {
            tracing::error!("无法启动文件传输的线程：{e}");
            self.finish(Err(Halt::Failed(XferError::protocol(
                self.trigger.protocol,
                format!("无法启动传输：{e}"),
            ))));
            self.pipe.stop();
            self.done.send_replace(true);
        }
    }

    /// 用户的回答。只在等待回答时接受，回答要与方向相符（下载给目录、上传给文件）。
    pub(crate) fn answer(&self, answer: Answer) -> Result<(), XferError> {
        match (&answer, self.trigger.upload) {
            (Answer::SaveTo(_), false) | (Answer::Send(_), true) | (Answer::Refuse, _) => {}
            _ => return Err(XferError::WrongAnswer),
        }
        let mut slot = self.answer.lock().unwrap();
        if !matches!(*slot, Slot::Pending) || self.cancelled.load(Ordering::Acquire) {
            return Err(XferError::NotWaiting);
        }
        *slot = Slot::Given(answer);
        self.answered.notify_all();
        Ok(())
    }

    /// 取消（用户点"取消"、在终端中按 Ctrl+C、关闭终端）。
    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.pipe.cancel();
        let _slot = self.answer.lock().unwrap();
        self.answered.notify_all();
    }

    /// 终端断开：取消，结果记为失败。
    pub(crate) fn disconnect(&self) {
        self.disconnected.store(true, Ordering::Release);
        self.cancel();
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// 已有结果（完成、失败或取消）：协议不会再读写，用户的输入可以照常发给远端。
    pub(crate) fn finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    pub(crate) fn info(&self) -> TransferInfo {
        self.tracker.info()
    }

    /// 传输结束（结果已上报、不再接受远端的输出）时变为 `true`。
    pub(crate) fn done_receiver(&self) -> watch::Receiver<bool> {
        self.done.subscribe()
    }

    fn run(self: Arc<Self>) {
        tracing::info!(
            id = %self.id,
            protocol = self.trigger.protocol.label(),
            upload = self.trigger.upload,
            "终端中的文件传输"
        );
        let result = match self.execute() {
            Err(_) if self.disconnected.load(Ordering::Acquire) => {
                Err(Halt::Failed(XferError::Disconnected))
            }
            Err(_) if self.is_cancelled() => Err(Halt::Cancelled),
            other => other,
        };
        self.finish(result);
        // 结束后不再接受远端的输出：之后的交还给终端（先取回剩下的再显示，顺序不乱）
        self.pipe.stop();
        self.done.send_replace(true);
    }

    fn zmodem_link(&self) -> zfiles::Link<'_> {
        zfiles::Link {
            pipe: &self.pipe,
            out: &*self.out,
            tracker: &self.tracker,
            escape_ctl: self.options.escape_ctl,
        }
    }

    fn execute(&self) -> Result<(), Halt> {
        let zlink = self.zmodem_link();
        let protocol = self.trigger.protocol;
        if protocol == Protocol::Zmodem && (self.options.conpty || self.options.tmux) {
            zfiles::abort(&zlink);
            return Err(Halt::Failed(if self.options.conpty {
                XferError::ZmodemOverConpty
            } else {
                XferError::ZmodemInTmux
            }));
        }
        // 等待回答
        self.tracker.update(true, |_| {});
        let answer = match self.wait_answer() {
            Waited::Answer(answer) => answer,
            Waited::NoAnswer => {
                self.refuse();
                return Err(Halt::Cancelled);
            }
            // 不回复远端：对面不是在等回答的协议程序，写过去的只会进 shell
            Waited::Overflow => {
                tracing::info!(id = %self.id, "等待回答期间远端输出太多，不是文件传输");
                return Err(Halt::Failed(XferError::protocol(
                    protocol,
                    "等待回答期间远端持续输出，不像文件传输，已停止识别",
                )));
            }
        };
        let cancelled = || self.is_cancelled();
        let tlink = || trzsz::Link {
            pipe: &self.pipe,
            out: &*self.out,
            tracker: &self.tracker,
            windows: self.trigger.win_server || self.options.conpty,
            cancelled: &cancelled,
        };
        match answer {
            Answer::SaveTo(dir) => {
                self.running(Some(dir.clone()));
                match protocol {
                    Protocol::Zmodem => zfiles::receive(&zlink, &dir),
                    Protocol::Trzsz => trzsz::receive(tlink(), &dir),
                }
            }
            Answer::Send(paths) => {
                self.running(None);
                match protocol {
                    Protocol::Zmodem => zfiles::send(&zlink, &paths),
                    Protocol::Trzsz => trzsz::send(tlink(), &paths, self.trigger.directory),
                }
            }
            Answer::Refuse => Err(Halt::Cancelled),
        }
    }

    fn running(&self, dir: Option<PathBuf>) {
        self.tracker.update(true, |i| {
            i.state = TransferState::Running;
            i.dir = dir;
        });
    }

    /// 等待用户选择。期间照常读出远端的输出、存起来，有了回答后放回管道交给协议；
    /// 没有回答或不是文件传输时也放回去（传输结束后交还给终端显示）。
    fn wait_answer(&self) -> Waited {
        let mut held: Vec<u8> = Vec::new();
        let waited = self.wait_answer_holding(&mut held);
        self.pipe.unread(&held);
        waited
    }

    fn wait_answer_holding(&self, held: &mut Vec<u8>) -> Waited {
        let deadline = Instant::now() + self.options.answer_timeout;
        let mut slot = self.answer.lock().unwrap();
        loop {
            if self.is_cancelled() {
                *slot = Slot::Closed;
                return Waited::NoAnswer;
            }
            if matches!(*slot, Slot::Given(_)) {
                let Slot::Given(answer) = std::mem::replace(&mut *slot, Slot::Closed) else {
                    unreachable!();
                };
                return match answer {
                    Answer::Refuse => Waited::NoAnswer,
                    Answer::Send(paths) if paths.is_empty() => Waited::NoAnswer,
                    answer => Waited::Answer(answer),
                };
            }
            while let Some(chunk) = self.pipe.try_next() {
                held.extend_from_slice(&chunk);
            }
            if held.len() > WAIT_BUFFER_LIMIT {
                *slot = Slot::Closed;
                return Waited::Overflow;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                *slot = Slot::Closed;
                return Waited::NoAnswer;
            }
            slot = self
                .answered
                .wait_timeout(slot, left.min(WAIT_POLL))
                .unwrap()
                .0;
        }
    }

    /// 告诉远端不传了：ZModem 发送中止序列，trzsz 回复不确认。
    fn refuse(&self) {
        match self.trigger.protocol {
            Protocol::Zmodem => zfiles::abort(&self.zmodem_link()),
            Protocol::Trzsz => {
                let cancelled = || true;
                trzsz::refuse(trzsz::Link {
                    pipe: &self.pipe,
                    out: &*self.out,
                    tracker: &self.tracker,
                    windows: self.trigger.win_server || self.options.conpty,
                    cancelled: &cancelled,
                });
            }
        }
    }

    /// 记录结果并上报。
    fn finish(&self, result: Result<(), Halt>) {
        if self.finished.swap(true, Ordering::AcqRel) {
            return;
        }
        let state = match result {
            Ok(()) => TransferState::Done,
            Err(Halt::Cancelled) => TransferState::Cancelled,
            Err(Halt::Failed(e)) => TransferState::Failed(e),
        };
        self.tracker.update(true, |i| i.state = state);
        let info = self.tracker.info();
        tracing::info!(
            id = %self.id,
            state = ?info.state,
            files = info.files,
            skipped = info.skipped.len(),
            "文件传输结束"
        );
    }
}
