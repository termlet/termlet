//! 把远端的输出交给传输协议。
//!
//! 写入的一方是包装终端字节接口的转发任务（异步，[`Pipe::push`]）：缓冲满时等待，形成对远端的反压；
//! 读取的一方是协议的线程（同步，[`Pipe::next`]），支持截止时间（ZModem 据此超时重发，trzsz 据此判断超时）。
//! 传输结束时 [`Pipe::stop`] 不再接受写入，[`Pipe::close`] 取回还没被协议读走的输出（应显示在终端中）。

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// 读取失败的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PipeError {
    /// 超过截止时间
    Timeout,
    /// 用户取消了传输
    Cancelled,
    /// 传输已结束（终端断开）
    Closed,
}

/// 缓冲的段数：会话每次最多送来几十 KB，足够应付协议处理文件时的短暂停顿。
pub(crate) const PIPE_CAPACITY: usize = 16;

pub(crate) struct Pipe {
    state: Mutex<State>,
    /// 有新的输出、取消、关闭时唤醒读取的一方
    readable: Condvar,
    /// 有空位、停止、关闭时唤醒写入的一方（异步与同步两种）
    writable: Notify,
    writable_sync: Condvar,
    capacity: usize,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Vec<u8>>,
    /// 上次读取剩下的、放回来的（下次先读它）
    pending: Vec<u8>,
    /// 不再接受写入（传输已结束，协议不会再读）；缓冲中的仍可由 close 取回
    stopped: bool,
    closed: bool,
    cancelled: bool,
}

impl Pipe {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::default(),
            readable: Condvar::new(),
            writable: Notify::new(),
            writable_sync: Condvar::new(),
            capacity: capacity.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wake_writers(&self) {
        self.writable.notify_waiters();
        self.writable_sync.notify_all();
    }

    /// 交给协议；缓冲满时等待。传输已结束时原样退回（应交还给终端）。
    pub(crate) async fn push(&self, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        if chunk.is_empty() {
            return Ok(());
        }
        loop {
            let notified = self.writable.notified();
            tokio::pin!(notified);
            // 先登记再检查，检查之后的唤醒不会漏掉
            notified.as_mut().enable();
            {
                let mut state = self.lock();
                if state.stopped || state.closed {
                    return Err(chunk);
                }
                if state.queue.len() < self.capacity {
                    state.queue.push_back(chunk);
                    self.readable.notify_all();
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    /// 与 [`Pipe::push`] 相同，在普通线程上等待（测试中模拟远端）。
    pub(crate) fn push_blocking(&self, chunk: Vec<u8>) -> Result<(), Vec<u8>> {
        if chunk.is_empty() {
            return Ok(());
        }
        let mut state = self.lock();
        loop {
            if state.stopped || state.closed {
                return Err(chunk);
            }
            if state.queue.len() < self.capacity {
                state.queue.push_back(chunk);
                self.readable.notify_all();
                return Ok(());
            }
            state = self
                .writable_sync
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }

    /// 读取下一段（先给放回来的）；`deadline` 为空时不限时。取消后立即返回 [`PipeError::Cancelled`]。
    pub(crate) fn next(&self, deadline: Option<Instant>) -> Result<Vec<u8>, PipeError> {
        let mut state = self.lock();
        loop {
            if state.cancelled {
                return Err(PipeError::Cancelled);
            }
            if let Some(chunk) = take_one(&mut state) {
                self.wake_writers();
                return Ok(chunk);
            }
            if state.closed {
                return Err(PipeError::Closed);
            }
            state = self.wait(state, deadline).ok_or(PipeError::Timeout)?;
        }
    }

    /// 不等待：有已收到的输出时取出一段。
    pub(crate) fn try_next(&self) -> Option<Vec<u8>> {
        let mut state = self.lock();
        if state.cancelled {
            return None;
        }
        let chunk = take_one(&mut state);
        if chunk.is_some() {
            self.wake_writers();
        }
        chunk
    }

    /// 等待新的变化，超过截止时间时返回 `None`。
    fn wait<'a>(
        &self,
        state: MutexGuard<'a, State>,
        deadline: Option<Instant>,
    ) -> Option<MutexGuard<'a, State>> {
        match deadline {
            None => Some(self.readable.wait(state).unwrap_or_else(|e| e.into_inner())),
            Some(deadline) => {
                let left = deadline.checked_duration_since(Instant::now())?;
                if left.is_zero() {
                    return None;
                }
                let (state, _) = self
                    .readable
                    .wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner());
                Some(state)
            }
        }
    }

    /// 把没用完的一段放回去，下次先读它。
    pub(crate) fn unread(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut state = self.lock();
        let mut pending = data.to_vec();
        pending.append(&mut state.pending);
        state.pending = pending;
        self.readable.notify_all();
    }

    /// 取消：之后的读取立即返回 [`PipeError::Cancelled`]（丢弃远端数据的 drain 不受影响）。
    pub(crate) fn cancel(&self) {
        self.lock().cancelled = true;
        self.readable.notify_all();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.lock().cancelled
    }

    /// 不再接受写入（传输结束时调用，之后的输出交还给终端）。
    pub(crate) fn stop(&self) {
        self.lock().stopped = true;
        self.wake_writers();
    }

    /// 结束：返回还没被协议读走的输出（应显示在终端中）；之后的读取返回 [`PipeError::Closed`]。
    pub(crate) fn close(&self) -> Vec<u8> {
        let mut state = self.lock();
        state.closed = true;
        let mut left = std::mem::take(&mut state.pending);
        for chunk in state.queue.drain(..) {
            left.extend_from_slice(&chunk);
        }
        self.readable.notify_all();
        self.wake_writers();
        left
    }

    /// 等下一段远端输出，不理会取消（丢弃数据时用）；`quiet` 内没有输出、到了 `end` 或已关闭时返回 `None`。
    fn wait_chunk(&self, quiet: Duration, end: Instant) -> Option<Vec<u8>> {
        let deadline = (Instant::now() + quiet).min(end);
        let mut state = self.lock();
        loop {
            if let Some(chunk) = state.queue.pop_front() {
                self.wake_writers();
                return Some(chunk);
            }
            if state.closed {
                return None;
            }
            state = self.wait(state, Some(deadline))?;
        }
    }

    fn take_pending(&self) -> Vec<u8> {
        std::mem::take(&mut self.lock().pending)
    }

    /// 丢弃已收到的输出，直到 `quiet` 内没有新的输出或超过 `limit`（trzsz 出错后、发送 `#fail` 之前清掉远端还在发的数据）。
    pub(crate) fn drain(&self, quiet: Duration, limit: Duration) {
        self.take_pending();
        let end = Instant::now() + limit;
        while Instant::now() < end && self.wait_chunk(quiet, end).is_some() {}
    }

    /// 中止 ZModem 后丢弃远端还在发的二进制数据，远端退出后的文本（程序的提示、shell 的提示符）交还给终端：
    ///
    /// - lrzsz 的 sz / rz 中止时回送一串 CAN（canit）：其后是远端程序退出后的输出，前面可能还夹着半个数据子包，
    ///   从末尾往前取连续的文本，放回后停止丢弃（实测 sz 回送的 CAN 之后还有一个空数据子包的 CRC，紧接着就是 shell 的输出）；
    /// - 整段都是文本（不含 ZDLE、是合法的 UTF-8）时同样交还，连同上一段末尾的文本；
    /// - 其余是二进制数据，丢弃。`quiet` 内没有新的输出（远端已结束）或超过 `limit` 时停止，最后一段末尾的文本交还
    ///   （实测 sz 有时不回送 CAN，最后一段是半截数据加 shell 的输出）。
    pub(crate) fn drain_binary(&self, quiet: Duration, limit: Duration) {
        let end = Instant::now() + limit;
        let mut last: Vec<u8> = Vec::new();
        let mut prev: Vec<u8> = Vec::new();
        // 返回 true 时停止
        let check = |buf: Vec<u8>, last: &mut Vec<u8>, prev: &mut Vec<u8>| -> bool {
            if let Some(i) = after_can_run(&buf) {
                self.unread(text_suffix(&buf[i..]));
                return true;
            }
            if !buf.contains(&0x18) && printable(&buf) && std::str::from_utf8(&buf).is_ok() {
                // 上一段末尾可能已是远端程序退出后的输出（例如 shell 执行的下一条命令），一并交还
                let mut text = text_suffix(prev).to_vec();
                text.extend_from_slice(&buf);
                self.unread(&text);
                return true;
            }
            *last = buf.clone();
            *prev = buf;
            false
        };
        let pending = self.take_pending();
        if !pending.is_empty() && check(pending, &mut last, &mut prev) {
            return;
        }
        loop {
            match self.wait_chunk(quiet, end) {
                Some(buf) => {
                    if check(buf, &mut last, &mut prev) {
                        return;
                    }
                }
                None => {
                    if !self.lock().closed {
                        self.unread(text_suffix(&last));
                    }
                    return;
                }
            }
        }
    }
}

fn take_one(state: &mut State) -> Option<Vec<u8>> {
    if !state.pending.is_empty() {
        return Some(std::mem::take(&mut state.pending));
    }
    state.queue.pop_front()
}

/// 最后一串连续 5 个以上 CAN（0x18）之后的位置，没有时为 `None`。
/// 数据中单个的 0x18 是 ZDLE 转义，连续 5 个只出现在中止序列中（对方据此判断中止，见 lrzsz 的 zgethdr）。
pub(crate) fn after_can_run(b: &[u8]) -> Option<usize> {
    let mut end = None;
    let mut i = 0;
    while i < b.len() {
        if b[i] != 0x18 {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < b.len() && b[j] == 0x18 {
            j += 1;
        }
        if j - i >= 5 {
            end = Some(j);
        }
        i = j;
    }
    end
}

/// 一个字节能否出现在终端文本中：可打印字符、换行、回车、制表、退格、响铃、ESC，以及 UTF-8 的多字节字符。
fn is_text(c: u8) -> bool {
    (c >= 0x20 && c != 0x7f) || matches!(c, 0x0d | 0x0a | 0x09 | 0x08 | 0x07 | 0x1b)
}

/// 从末尾往前取连续的文本，去掉开头不完整的 UTF-8 字节（CRC 等二进制字节恰好大于 0x7f 时）。
pub(crate) fn text_suffix(b: &[u8]) -> &[u8] {
    let mut i = b.len();
    while i > 0 && is_text(b[i - 1]) {
        i -= 1;
    }
    let mut suffix = &b[i..];
    while !suffix.is_empty() && std::str::from_utf8(suffix).is_err() {
        suffix = &suffix[1..];
    }
    suffix
}

/// 一段输出是否像终端文本：控制字符（换行、回车、制表、退格、响铃、ESC 除外）不超过 5%。
fn printable(b: &[u8]) -> bool {
    let ctrl = b.iter().filter(|&&c| !is_text(c)).count();
    ctrl * 20 <= b.len()
}
