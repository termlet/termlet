//! 终端会话的字节接口：传输（本地 PTY、SSH 通道、Telnet、串口）与终端之间，字节进、字节出。
//!
//! 本地 PTY、SSH 通道、Telnet、串口都在 `termlet-core` 中实现这个接口；终端只处理 UTF-8，
//! 编码转换（GBK、Big5）在传输一侧完成。

/// 终端的网格大小（字符格）与像素大小（部分程序用 `CSI 14 t` 查询）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridSize {
    pub columns: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl GridSize {
    pub fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            cell_width: 8,
            cell_height: 16,
        }
    }

    pub fn pixel_width(&self) -> u16 {
        self.columns.saturating_mul(self.cell_width)
    }

    pub fn pixel_height(&self) -> u16 {
        self.rows.saturating_mul(self.cell_height)
    }
}

impl Default for GridSize {
    fn default() -> Self {
        Self::new(80, 24)
    }
}

/// 传输送给终端的事件。
#[derive(Debug)]
pub enum IoEvent {
    /// 远端的输出（UTF-8）
    Output(Vec<u8>),
    /// 会话结束：进程退出、连接断开
    Closed(CloseReason),
}

/// 会话为什么结束。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseReason {
    /// 程序正常退出（退出码，拿不到时为 `None`）
    Exited(Option<i32>),
    /// 连接意外断开（网络中断、服务器没有报告退出就关闭了通道）：可以自动重连
    Lost(String),
    /// 出错，重试也不会好（给日志看的原因）
    Failed(String),
}

/// 终端发给传输的操作。实现方不能阻塞调用者（界面线程）：写入放进队列，由传输自己的线程或任务发送。
pub trait TerminalInput: Send + Sync + 'static {
    /// 写入用户的输入或终端的应答（光标位置报告等）。
    fn write(&self, data: &[u8]);
    /// 终端大小变了。
    fn resize(&self, size: GridSize);
    /// 终端不再使用这个传输（Tab 关闭、重连换上了新的传输）：结束会话（本地 shell 结束进程，SSH 关闭通道）。
    /// 之后输出通道会随之结束。不能只靠丢弃：终端的引擎线程一直持有输入句柄（写终端的应答）。
    fn close(&self) {}
    /// 切换远端的编码（`utf-8`、`gb18030`、`big5`），之后的输入输出按新的编码转换；
    /// 不转换编码的传输（本地 PTY）忽略。
    fn set_encoding(&self, _encoding: &str) {}
    /// 终端中的文件传输（ZModem、trzsz）开始（`true`）与结束（`false`）时调用：期间输入输出原样收发，
    /// 不做编码转换（Telnet、串口还要停掉回车键映射与本地回显），否则二进制数据会被改写。
    /// 不转换数据的传输（本地 PTY）忽略。
    fn set_binary(&self, _on: bool) {}
    /// 传输会改写数据中的控制字符（没有协商二进制模式的 Telnet：服务器按网络虚拟终端的规则改写回车与 NUL）：
    /// 终端中的 ZModem 要请双方转义全部控制字符，否则数据被改坏。
    fn mangles_control_bytes(&self) -> bool {
        false
    }
}

/// 一个终端会话的两端。
pub struct TerminalIo {
    /// 传输送来的输出与结束通知；通道是有界的，解析跟不上时对传输形成背压
    pub output: async_channel::Receiver<IoEvent>,
    pub input: std::sync::Arc<dyn TerminalInput>,
}
