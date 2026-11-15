//! 终端输出的旁路：会话日志、会话录制、sudo 密码提示检测、在终端中执行命令（撰写栏的“执行并对比”、
//! `termlet exec --terminal`）都要看远端的原始输出。
//!
//! 由终端的引擎线程调用（[`termlet_terminal::TerminalEngine::set_output_observer`]），要很轻：
//! 日志、录制的写入只是交给它们的后台线程，密码提示检测只看最后一行。检测到的变化经通道送回界面线程。
//! 录制还要知道终端大小的变化：每个传输的输入端经 [`OutputTap::wrap_input`] 包一层，引擎改大小时记下。

use std::sync::{Arc, Mutex, MutexGuard};

use termlet_core::recording::{self, Recording};
use termlet_core::session_log::{Halt, SessionLog};
use termlet_core::sudo::{FillTicket, PasswordWatch, PromptEvent};
use termlet_core::termrun::TermRun;
use termlet_terminal::{GridSize, OutputObserver, TerminalInput};

/// 送回界面线程的变化。
pub enum TapEvent {
    /// sudo 密码提示出现或过去了
    Password(PromptEvent),
    /// 会话日志自己停下了（写入失败、达到大小上限）
    LogHalted(Halt),
    /// 录制自己停下了（写入失败、达到大小上限）
    RecordingHalted(recording::Halt),
}

pub struct OutputTap {
    log: Mutex<Option<Arc<SessionLog>>>,
    recording: Mutex<Option<Arc<Recording>>>,
    password: Mutex<Option<PasswordWatch>>,
    /// 进行中的在终端中执行（最多一个）
    run: Mutex<Option<Arc<TermRun>>>,
    events: async_channel::Sender<TapEvent>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl OutputTap {
    pub fn new() -> (Arc<Self>, async_channel::Receiver<TapEvent>) {
        let (tx, rx) = async_channel::unbounded();
        let tap = Arc::new(Self {
            log: Mutex::new(None),
            recording: Mutex::new(None),
            password: Mutex::new(None),
            run: Mutex::new(None),
            events: tx,
        });
        (tap, rx)
    }

    /// 交给终端引擎的观察者。
    pub fn observer(self: &Arc<Self>) -> OutputObserver {
        let tap = self.clone();
        Arc::new(move |bytes: &[u8]| tap.on_output(bytes))
    }

    fn on_output(&self, bytes: &[u8]) {
        if let Some(log) = lock(&self.log).as_ref() {
            log.write(bytes);
        }
        if let Some(recording) = lock(&self.recording).as_ref() {
            recording.output(bytes);
        }
        if let Some(run) = lock(&self.run).as_ref() {
            run.feed(bytes);
        }
        let event = lock(&self.password)
            .as_mut()
            .and_then(|watch| watch.feed(bytes));
        if let Some(event) = event {
            let _ = self.events.try_send(TapEvent::Password(event));
        }
    }

    /// 正在记录的会话日志。
    pub fn log(&self) -> Option<Arc<SessionLog>> {
        lock(&self.log).clone()
    }

    /// 开始（`Some`）或停止（`None`，丢弃即停止）记录会话日志。
    pub fn set_log(&self, log: Option<Arc<SessionLog>>) {
        *lock(&self.log) = log;
    }

    /// 正在进行的录制。
    pub fn recording(&self) -> Option<Arc<Recording>> {
        lock(&self.recording).clone()
    }

    /// 开始（`Some`）或结束（`None`）录制，返回原来的那个（调用方停止它）。开始时要与屏幕的快照在同一时刻
    /// （[`termlet_terminal::TerminalEngine::screen_dump_then`]），录制才从快照接上。
    pub fn set_recording(&self, recording: Option<Arc<Recording>>) -> Option<Arc<Recording>> {
        std::mem::replace(&mut *lock(&self.recording), recording)
    }

    /// 录制自己停下时的回调（在录制的后台线程上调用）。
    pub fn recording_halt_reporter(
        self: &Arc<Self>,
    ) -> impl FnOnce(recording::Halt) + Send + 'static {
        let events = self.events.clone();
        move |halt| {
            let _ = events.try_send(TapEvent::RecordingHalted(halt));
        }
    }

    /// 交给终端视图的输入观察者（录制同时记录输入时）：用户的键入与粘贴交给正在进行的录制。
    pub fn input_observer(self: &Arc<Self>) -> OutputObserver {
        let tap = self.clone();
        Arc::new(move |bytes: &[u8]| {
            if let Some(recording) = lock(&tap.recording).as_ref() {
                recording.input(bytes);
            }
        })
    }

    /// 包一层传输的输入端：引擎改终端大小时记进正在进行的录制，其余原样转给传输。
    pub fn wrap_input(self: &Arc<Self>, inner: Arc<dyn TerminalInput>) -> Arc<dyn TerminalInput> {
        Arc::new(TapInput {
            inner,
            tap: self.clone(),
        })
    }

    /// 日志自己停下时的回调（在日志的后台线程上调用）。
    pub fn halt_reporter(self: &Arc<Self>) -> impl FnOnce(Halt) + Send + 'static {
        let events = self.events.clone();
        move |halt| {
            let _ = events.try_send(TapEvent::LogHalted(halt));
        }
    }

    /// 检测这个登录用户的 sudo 密码提示；`None` 时不检测。
    pub fn watch_password(&self, user: Option<String>) {
        *lock(&self.password) = user.map(PasswordWatch::new);
    }

    /// 是否正停在密码提示上、还没有填。
    pub fn password_pending(&self) -> bool {
        lock(&self.password)
            .as_ref()
            .is_some_and(PasswordWatch::is_pending)
    }

    /// 用户自己开始输入：不再填。
    pub fn typed(&self) {
        if let Some(watch) = lock(&self.password).as_mut() {
            watch.typed();
        }
    }

    /// 开始（`Some`）或结束（`None`）在终端中执行：把输出交给它找标记。
    pub fn set_run(&self, run: Option<Arc<TermRun>>) {
        *lock(&self.run) = run;
    }

    /// 是否正在终端中执行。
    pub fn running(&self) -> bool {
        lock(&self.run).is_some()
    }

    /// 要填入密码时取走等待标记（同一个提示只填一次）。
    pub fn take_fill(&self) -> Option<FillTicket> {
        lock(&self.password).as_mut().and_then(PasswordWatch::take)
    }
}

/// 传输的输入端外面的一层：终端大小的变化记进录制（写入不记：其中有终端的应答、代填的密码）。
struct TapInput {
    inner: Arc<dyn TerminalInput>,
    tap: Arc<OutputTap>,
}

impl TerminalInput for TapInput {
    fn write(&self, data: &[u8]) {
        self.inner.write(data);
    }

    fn resize(&self, size: GridSize) {
        if let Some(recording) = lock(&self.tap.recording).as_ref() {
            recording.resize(size.columns, size.rows);
        }
        self.inner.resize(size);
    }

    fn close(&self) {
        self.inner.close();
    }

    fn set_encoding(&self, encoding: &str) {
        self.inner.set_encoding(encoding);
    }

    fn set_binary(&self, on: bool) {
        self.inner.set_binary(on);
    }

    fn mangles_control_bytes(&self) -> bool {
        self.inner.mangles_control_bytes()
    }
}
