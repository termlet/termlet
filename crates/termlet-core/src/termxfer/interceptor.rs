//! 夹在传输（SSH、本地终端、Telnet、串口）与终端之间的过滤层：包装 [`TerminalIo`]，对终端看起来仍是 `TerminalIo`。
//!
//! 转发任务（核心的运行时上）把远端的输出先交给检测器：检测到起始序列后，之后的输出交给传输协议（不显示），
//! 传输结束后协议没读走的输出与之后的输出照常交给终端；期间用户的输入（除 Ctrl+C 取消外）不发给远端。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use termlet_io::{GridSize, IoEvent, TerminalInput, TerminalIo};
use tokio::sync::watch;

use super::detect::{Detector, Fed, Trigger};
use super::info::{Answer, TransferId, TransferInfo, TransferState, XferError};
use super::transfer::{Transfer, TransferOptions};

/// 包装后的输出通道的容量（与各传输自己的相同）。
const OUTPUT_CAPACITY: usize = 64;
/// 传输没有成功结束后，这么久之内不检测 ZModem（远端的 sz 可能在收到中止序列前重发了起始帧）。
const ZMODEM_QUIET: Duration = Duration::from_secs(2);
/// 终端断开时最多等传输收尾多久。
const DISCONNECT_WAIT: Duration = Duration::from_secs(5);

/// 拦截的参数。
#[derive(Clone, Debug)]
pub struct InterceptOptions {
    /// 识别终端中的文件传输（设置 [`TerminalSettings::file_transfer`](crate::model::TerminalSettings::file_transfer)）；
    /// 之后可以用 [`Interceptor::set_enabled`] 修改
    pub enabled: bool,
    /// 等待用户选择文件或保存位置的最长时间，超时视为取消
    pub answer_timeout: Duration,
    /// 上报进度的最短间隔
    pub progress_every: Duration,
    /// 起始序列的开头被拆开时暂扣输出的最长时间
    pub hold_delay: Duration,
}

impl Default for InterceptOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            answer_timeout: Duration::from_secs(5 * 60),
            progress_every: Duration::from_millis(200),
            hold_delay: Duration::from_millis(30),
        }
    }
}

/// 被包装的传输是什么样的（决定 ZModem 能不能用、trzsz 用哪种行协议）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkInfo {
    /// 经过 Windows 的伪终端（本地终端、WSL）：ConPTY 会改写二进制数据，ZModem 不能用，trzsz 用 Windows 的行协议
    pub conpty: bool,
    /// 终端在 tmux 中（主机开启了 tmux 续连且已接入，[`TmuxStatus::Attached`](crate::ssh::TmuxStatus::Attached)）：
    /// tmux 不转发 ZModem 的数据
    pub tmux: bool,
}

/// 一个终端的文件传输。每个终端 Tab 一个，跨重连不变：每次连接（重连）得到的 [`TerminalIo`] 都经
/// [`Interceptor::wrap`] 包装后再交给终端。可以克隆，克隆的是同一个。
#[derive(Clone)]
pub struct Interceptor(Arc<Shared>);

struct Shared {
    options: InterceptOptions,
    enabled: AtomicBool,
    subscribers: Mutex<Vec<async_channel::Sender<TransferInfo>>>,
    /// 最近一次传输（回答、取消、查询用）
    current: Mutex<Option<Arc<Transfer>>>,
}

impl Shared {
    fn broadcast(&self, info: TransferInfo) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| tx.try_send(info.clone()).is_ok());
    }

    fn current(&self) -> Option<Arc<Transfer>> {
        self.current.lock().unwrap().clone()
    }
}

impl Interceptor {
    pub fn new(options: InterceptOptions) -> Self {
        Self(Arc::new(Shared {
            enabled: AtomicBool::new(options.enabled),
            options,
            subscribers: Mutex::default(),
            current: Mutex::default(),
        }))
    }

    /// 订阅传输的状态：开始等待回答、开始传输、进度（最多每 200 毫秒一次）、结束时各收到一份完整的快照。
    /// 每次调用得到一个独立的接收端，不再接收时丢弃即可。
    pub fn events(&self) -> async_channel::Receiver<TransferInfo> {
        let (tx, rx) = async_channel::unbounded();
        self.0.subscribers.lock().unwrap().push(tx);
        rx
    }

    /// 包装一个终端会话：返回交给终端的那一端。转发在核心的运行时上进行，不阻塞调用方。
    pub fn wrap(&self, io: TerminalIo, link: LinkInfo) -> TerminalIo {
        let (tx, rx) = async_channel::bounded(OUTPUT_CAPACITY);
        let state = Arc::new(LinkState {
            inner: io.input,
            active: Mutex::new(None),
            info: link,
        });
        let pump = Pump {
            shared: self.0.clone(),
            link: state.clone(),
            out: tx,
            detector: Detector::new(),
            hold: None,
        };
        crate::runtime::spawn(pump.run(io.output));
        TerminalIo {
            output: rx,
            input: Arc::new(GuardedInput { link: state }),
        }
    }

    /// 打开或关闭识别（设置改变时调用）；进行中的传输不受影响。
    pub fn set_enabled(&self, enabled: bool) {
        self.0.enabled.store(enabled, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Acquire)
    }

    /// 最近一次传输的状态（进行中的，或已结束的结果）；还没有过传输时为 `None`。
    pub fn current(&self) -> Option<TransferInfo> {
        self.0.current().map(|t| t.info())
    }

    /// 正在等待选择要上传的文件时返回它的标识：界面据此接收拖到终端上的文件（[`Answer::Send`]）。
    pub fn accepts_drop(&self) -> Option<TransferId> {
        let t = self.0.current()?;
        let info = t.info();
        (info.upload && info.state == TransferState::Waiting && !t.finished()).then_some(t.id)
    }

    /// 回答传输请求：保存目录、要上传的文件或拒绝。
    pub fn answer(&self, id: TransferId, answer: Answer) -> Result<(), XferError> {
        let t = self
            .0
            .current()
            .filter(|t| t.id == id)
            .ok_or(XferError::NotWaiting)?;
        t.answer(answer)
    }

    /// 取消传输（等待回答时等于拒绝）；没有这个传输或它已结束时返回 `false`。
    pub fn cancel(&self, id: TransferId) -> bool {
        match self.0.current().filter(|t| t.id == id && !t.finished()) {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }
}

/// 一个被包装的传输。
struct LinkState {
    inner: Arc<dyn TerminalInput>,
    /// 进行中的传输：期间远端的输出交给它
    active: Mutex<Option<Arc<Transfer>>>,
    info: LinkInfo,
}

impl LinkState {
    fn active(&self) -> Option<Arc<Transfer>> {
        self.active.lock().unwrap().clone()
    }
}

/// 交给终端的输入：传输期间只认 Ctrl+C（取消传输），其余输入丢弃（会混进传输的数据）。
struct GuardedInput {
    link: Arc<LinkState>,
}

impl TerminalInput for GuardedInput {
    fn write(&self, data: &[u8]) {
        if let Some(t) = self.link.active()
            && !t.finished()
        {
            if data.contains(&0x03) {
                t.cancel();
            }
            return;
        }
        self.link.inner.write(data);
    }

    fn resize(&self, size: GridSize) {
        self.link.inner.resize(size);
    }

    fn close(&self) {
        if let Some(t) = self.link.active() {
            t.cancel();
        }
        self.link.inner.close();
    }

    fn set_encoding(&self, encoding: &str) {
        self.link.inner.set_encoding(encoding);
    }

    fn set_binary(&self, on: bool) {
        self.link.inner.set_binary(on);
    }

    fn mangles_control_bytes(&self) -> bool {
        self.link.inner.mangles_control_bytes()
    }
}

/// 转发任务。
struct Pump {
    shared: Arc<Shared>,
    link: Arc<LinkState>,
    out: async_channel::Sender<IoEvent>,
    detector: Detector,
    /// 暂扣的输出到这个时间还等不到后续就显示出来
    hold: Option<Instant>,
}

async fn transfer_done(done: Option<watch::Receiver<bool>>) {
    match done {
        Some(mut done) => {
            let _ = done.wait_for(|d| *d).await;
        }
        None => std::future::pending().await,
    }
}

async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

impl Pump {
    async fn run(mut self, input: async_channel::Receiver<IoEvent>) {
        loop {
            let done = self.link.active().map(|t| t.done_receiver());
            let hold = self.hold;
            tokio::select! {
                event = input.recv() => match event {
                    Ok(IoEvent::Output(bytes)) => {
                        if !self.output(bytes).await {
                            // 终端不要输出了
                            if let Some(t) = self.link.active() {
                                t.cancel();
                            }
                            return;
                        }
                    }
                    Ok(IoEvent::Closed(reason)) => {
                        self.end().await;
                        let _ = self.out.send(IoEvent::Closed(reason)).await;
                        return;
                    }
                    Err(_) => {
                        self.end().await;
                        return;
                    }
                },
                _ = transfer_done(done) => {
                    self.detach().await;
                }
                _ = until(hold) => {
                    // 等不到后续：暂扣的是完整的 trzsz 触发行（远端输出后就等回答）时开始传输，否则照常显示
                    self.hold = None;
                    let fed = if self.shared.enabled.load(Ordering::Acquire) {
                        self.detector.settle()
                    } else {
                        Fed {
                            show: self.detector.flush(),
                            ..Fed::default()
                        }
                    };
                    self.show(fed.show).await;
                    if let Some(trigger) = fed.trigger {
                        self.start(trigger, fed.rest);
                    }
                }
            }
        }
    }

    /// 显示一段输出；终端已不接收时返回 `false`。
    async fn show(&self, bytes: Vec<u8>) -> bool {
        bytes.is_empty() || self.out.send(IoEvent::Output(bytes)).await.is_ok()
    }

    /// 处理一段远端输出。
    async fn output(&mut self, mut bytes: Vec<u8>) -> bool {
        if let Some(t) = self.link.active() {
            match t.pipe.push(bytes).await {
                Ok(()) => return true,
                // 传输已结束：先显示它没读走的，再按普通输出处理这一段
                Err(back) => {
                    if !self.detach().await {
                        return false;
                    }
                    bytes = back;
                }
            }
        }
        if !self.shared.enabled.load(Ordering::Acquire) {
            self.hold = None;
            let held = self.detector.flush();
            return self.show(held).await && self.show(bytes).await;
        }
        let fed = self.detector.feed(bytes);
        if !self.show(fed.show).await {
            return false;
        }
        if let Some(trigger) = fed.trigger {
            self.hold = None;
            self.start(trigger, fed.rest);
        } else if self.detector.holding() {
            self.hold = Some(Instant::now() + self.shared.options.hold_delay);
        } else {
            self.hold = None;
        }
        true
    }

    /// 开始传输：之后远端的输出交给它，它直接写远端的输入；状态经事件送给界面。
    fn start(&mut self, trigger: Trigger, rest: Vec<u8>) {
        let options = TransferOptions {
            answer_timeout: self.shared.options.answer_timeout,
            progress_every: self.shared.options.progress_every,
            conpty: self.link.info.conpty,
            tmux: self.link.info.tmux,
            escape_ctl: self.link.inner.mangles_control_bytes(),
        };
        let shared: Weak<Shared> = Arc::downgrade(&self.shared);
        let report = Arc::new(move |info: TransferInfo| {
            if let Some(shared) = shared.upgrade() {
                shared.broadcast(info);
            }
        });
        let t = Transfer::new(trigger, rest, self.link.inner.clone(), options, report);
        // 期间数据原样收发（不转换编码）
        self.link.inner.set_binary(true);
        *self.link.active.lock().unwrap() = Some(t.clone());
        *self.shared.current.lock().unwrap() = Some(t.clone());
        t.begin();
    }

    /// 摘下已结束的传输，显示它没读走的输出（远端程序退出后输出的提示符等）。
    async fn detach(&mut self) -> bool {
        let Some(t) = self.link.active.lock().unwrap().take() else {
            return true;
        };
        let left = t.pipe.close();
        if t.info().state != TransferState::Done {
            self.detector.suppress_zmodem(ZMODEM_QUIET);
        }
        self.link.inner.set_binary(false);
        self.show(left).await
    }

    /// 终端断开：传输随之结束（记为失败），显示剩下的输出。
    async fn end(&mut self) {
        if let Some(t) = self.link.active() {
            t.disconnect();
            let mut done = t.done_receiver();
            let _ = tokio::time::timeout(DISCONNECT_WAIT, done.wait_for(|d| *d)).await;
            self.detach().await;
        }
        let held = self.detector.flush();
        self.show(held).await;
    }
}
