//! 终端中文件传输的测试：检测起始序列、本机文件、trzsz，ZModem 两端互相收发的回环测试，
//! 经 [`Interceptor`] 包装的完整流程（远端用本模块的协议实现模拟 sz / rz / tsz），
//! 以及对真实 sshd 的集成测试（设置 `TERMLET_TEST_SSH` 后运行）。

mod detect;
mod files;
mod interceptor;
mod sshd;
mod trzsz;
mod zmodem;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use termlet_io::{GridSize, TerminalInput};

use super::info::{Protocol, TransferId, TransferInfo};
use super::pipe::Pipe;
use super::progress::Tracker;

/// 写入另一端的管道（模拟线路的一个方向）；同时记下写过的内容。
pub(super) struct PipeOut {
    pub pipe: Arc<Pipe>,
    pub written: Mutex<Vec<u8>>,
}

impl PipeOut {
    pub(super) fn new(pipe: Arc<Pipe>) -> Arc<Self> {
        Arc::new(Self {
            pipe,
            written: Mutex::default(),
        })
    }

    pub(super) fn written(&self) -> Vec<u8> {
        self.written.lock().unwrap().clone()
    }
}

impl TerminalInput for PipeOut {
    fn write(&self, data: &[u8]) {
        self.written.lock().unwrap().extend_from_slice(data);
        let _ = self.pipe.push_blocking(data.to_vec());
    }

    fn resize(&self, _size: GridSize) {}
}

/// 足够大、写入不会阻塞的管道。
pub(super) fn wide_pipe() -> Arc<Pipe> {
    Arc::new(Pipe::new(1 << 20))
}

/// 不上报的进度记录。
pub(super) fn quiet_tracker(protocol: Protocol, upload: bool) -> Tracker {
    Tracker::new(
        TransferInfo::new(TransferId::next(), protocol, upload, false),
        Duration::ZERO,
        Arc::new(|_| {}),
    )
}

pub(super) fn write_file(path: &Path, content: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

pub(super) fn read_file(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("读取 {} 失败：{e}", path.display()))
}

/// 目录中的名称（排序后）。
pub(super) fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// 伪随机数据（可重复）。
pub(super) fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}
