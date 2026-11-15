//! 串口：列出本机的串口（含 USB 标识）、按参数打开（波特率、数据位、停止位、校验、流控）。
//! Rust 的 `serialport` crate 是 MPL-2.0，这里自己实现：
//! Windows 用通信 API（重叠 I/O，读写可以同时进行）与 SetupAPI，Linux、macOS 用 termios。
//!
//! USB 串口换了插口、端口号变了时，按记下的 VID / PID / 序列号找回端口（[`resolve`]）。拔掉后会话按断线结束，
//! 界面自动重连，插回即恢复。有的 USB 串口驱动在设备拔掉后不报错、只是一直读不到数据：空闲时查询线路状态，
//! 查询失败且端口已不在系统的串口列表中时认为已拔掉。
//!
//! 全部是阻塞调用（列出串口要查询设备管理器，几到几十毫秒）：在后台线程调用；打开终端见
//! [`crate::rawterm::open`]（它在核心的运行时上另开阻塞任务）。

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as sys;
#[cfg(windows)]
use windows as sys;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::model::{FlowControl, Host, Parity, StopBits, UsbId};
use crate::rawterm::{Wire, WireEvent};

/// 本机的一个串口。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PortInfo {
    /// 端口名：Windows 的 `COM3`，Unix 的 `/dev/ttyUSB0`
    pub name: String,
    /// 系统中的名称（例如设备管理器中的 "USB-SERIAL CH340 (COM3)"）；没有时为空
    pub description: String,
    /// USB 串口的标识（保存主机时记下，端口号变了时据此找回）
    pub usb: Option<UsbId>,
}

/// 要打开的串口与参数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SerialConfig {
    /// 端口名（主机的地址）
    pub port: String,
    pub baud: u32,
    pub data_bits: u8,
    pub parity: Parity,
    pub stop_bits: StopBits,
    pub flow_control: FlowControl,
    /// 设备的 USB 标识（选择端口时记下）
    pub usb: Option<UsbId>,
}

impl SerialConfig {
    /// 主机设置中的串口与参数。
    pub fn for_host(host: &Host) -> Self {
        let serial = &host.serial;
        Self {
            port: host.address.clone(),
            baud: serial.baud,
            data_bits: serial.data_bits,
            parity: serial.parity,
            stop_bits: serial.stop_bits,
            flow_control: serial.flow_control,
            usb: serial.usb.clone(),
        }
    }
}

/// 串口的错误。消息是写给日志看的，界面按变体选用自己的文案。
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SerialError {
    /// 端口不在（设备没有插上）：可以稍后重试，插回即恢复
    #[error("找不到串口 {port}，请检查设备是否已连接")]
    NotFound { port: String },
    /// 被其他程序占用：可以稍后重试
    #[error("串口 {port} 被其他程序占用")]
    Busy { port: String },
    #[error("没有权限打开串口 {port}（Linux 上需要把用户加入 dialout 组）")]
    PermissionDenied { port: String },
    /// 这个串口（或这个平台）不支持这组参数：重试也不会好
    #[error("串口 {port} 不支持这组参数：{reason}")]
    Unsupported { port: String, reason: String },
    /// 打开后设备被拔掉
    #[error("串口 {port} 已断开")]
    Disconnected { port: String },
    #[error("打开串口 {port} 失败：{reason}")]
    Io { port: String, reason: String },
    #[error("列出串口失败：{0}")]
    List(String),
}

impl SerialError {
    /// 自动重连是否值得一试：设备没插上、被占用、拔掉了，插回或释放后就好；没有权限、参数不受支持不会好。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            SerialError::NotFound { .. }
                | SerialError::Busy { .. }
                | SerialError::Disconnected { .. }
                | SerialError::Io { .. }
        )
    }
}

/// 列出本机的串口，按端口号排序（COM2 在 COM10 之前）。
pub fn list_ports() -> Result<Vec<PortInfo>, SerialError> {
    let mut ports = sys::list()?;
    sort_ports(&mut ports);
    Ok(ports)
}

/// 按端口号排序：名字短的在前（COM2 在 COM10 之前），一样长时按字母。
fn sort_ports(ports: &mut [PortInfo]) {
    ports.sort_by(|a, b| a.name.len().cmp(&b.name.len()).then(a.name.cmp(&b.name)));
}

fn in_list(ports: &[PortInfo], name: &str) -> bool {
    ports.iter().any(|p| p.name.eq_ignore_ascii_case(name))
}

/// 确定要打开的端口：先按序列号找同一个设备，再用记下的端口号，最后找唯一一个 VID / PID 相同的设备；
/// 列表中都没有时（例如虚拟串口）按端口号打开。
pub fn resolve(config: &SerialConfig, ports: &[PortInfo]) -> String {
    let same_usb = |p: &PortInfo| match (&p.usb, &config.usb) {
        (Some(have), Some(want)) => {
            !want.vid.is_empty()
                && have.vid.eq_ignore_ascii_case(&want.vid)
                && have.pid.eq_ignore_ascii_case(&want.pid)
        }
        _ => false,
    };
    if let Some(want) = &config.usb
        && !want.serial_number.is_empty()
        && let Some(port) = ports.iter().find(|p| {
            same_usb(p)
                && p.usb
                    .as_ref()
                    .is_some_and(|u| u.serial_number == want.serial_number)
        })
    {
        return port.name.clone();
    }
    if let Some(port) = ports
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(&config.port))
    {
        return port.name.clone();
    }
    let mut matches = ports.iter().filter(|p| same_usb(p));
    match (matches.next(), matches.next()) {
        (Some(only), None) => only.name.clone(),
        _ => config.port.clone(),
    }
}

/// 打开时被占用就稍等再试一次：重连时上一个连接的读写线程可能还没放下端口。
const BUSY_RETRY: Duration = Duration::from_millis(300);

/// 打开的端口与它是否在系统的串口列表中（不在列表中的虚拟串口空闲时不检查是否已拔掉）。
fn open_port(config: &SerialConfig) -> Result<(sys::Port, String, bool), SerialError> {
    let (name, listed) = match sys::list() {
        Ok(ports) => {
            let name = resolve(config, &ports);
            let listed = in_list(&ports, &name);
            (name, listed)
        }
        Err(e) => {
            tracing::debug!("列出串口失败，按端口号打开：{e}");
            (config.port.clone(), false)
        }
    };
    let port = match sys::Port::open(&name, config) {
        Err(SerialError::Busy { .. }) => {
            std::thread::sleep(BUSY_RETRY);
            sys::Port::open(&name, config)?
        }
        other => other?,
    };
    Ok((port, name, listed))
}

/// 测试连接：按参数打开串口，随即关闭；返回耗时。
pub fn test_open(config: &SerialConfig) -> Result<Duration, SerialError> {
    let started = Instant::now();
    let (port, _, _) = open_port(config)?;
    drop(port);
    Ok(started.elapsed().max(Duration::from_millis(1)))
}

/// 读缓冲的大小。
const READ_BUFFER: usize = 16 * 1024;

/// 打开的串口作为行规程的传输层：读写各一个线程（与本地终端相同），界面线程从不阻塞。
pub(crate) struct SerialWire {
    /// 读写线程持有端口；两个线程都结束后端口随之关闭（重连时不会因为旧的连接还开着而被占用）
    port: Weak<sys::Port>,
    queue: Mutex<std::sync::mpsc::Sender<Vec<u8>>>,
    closed: Arc<AtomicBool>,
}

impl SerialWire {
    /// 打开串口，返回传输层、它的事件与实际打开的端口名（按 USB 标识找回时与设置中的不同）。
    pub(crate) fn start(
        config: &SerialConfig,
    ) -> Result<(Arc<Self>, mpsc::Receiver<WireEvent>, String), SerialError> {
        let (port, name, listed) = open_port(config)?;
        tracing::info!(port = %name, baud = config.baud, "串口已打开");
        let port = Arc::new(port);
        let closed = Arc::new(AtomicBool::new(false));
        let (events_tx, events) = mpsc::channel::<WireEvent>(16);
        let (queue, frames) = std::sync::mpsc::channel::<Vec<u8>>();

        let reader_port = port.clone();
        let reader_closed = closed.clone();
        let reader_queue = queue.clone();
        let reader_name = name.clone();
        let _ = std::thread::Builder::new()
            .name("serial-reader".into())
            .spawn(move || {
                let end = read_loop(
                    &reader_port,
                    &reader_name,
                    listed,
                    &reader_closed,
                    &events_tx,
                );
                // 读的一端结束（拔掉、出错、关闭）：写线程随之结束，两个线程都放下后端口随即关闭，
                // 界面重连时不会因为这个连接还开着而被占用
                reader_closed.store(true, Ordering::SeqCst);
                reader_port.cancel();
                let _ = reader_queue.send(Vec::new());
                drop(reader_port);
                tracing::info!(port = %reader_name, "串口的读取已结束");
                if let Some(event) = end {
                    let _ = events_tx.blocking_send(event);
                }
            });

        let writer_port = port.clone();
        let writer_closed = closed.clone();
        let _ = std::thread::Builder::new()
            .name("serial-writer".into())
            .spawn(move || {
                while let Ok(data) = frames.recv() {
                    if writer_closed.load(Ordering::SeqCst) {
                        break;
                    }
                    if let Err(e) = writer_port.write_all(&data) {
                        tracing::debug!("写串口失败：{e}");
                        break;
                    }
                }
            });

        Ok((
            Arc::new(Self {
                port: Arc::downgrade(&port),
                queue: Mutex::new(queue),
                closed,
            }),
            events,
            name,
        ))
    }
}

/// 读到结束：返回最后要报告的事件（主动关闭时为 `None`）。
fn read_loop(
    port: &sys::Port,
    name: &str,
    mut listed: bool,
    closed: &AtomicBool,
    events: &mpsc::Sender<WireEvent>,
) -> Option<WireEvent> {
    let mut buf = vec![0u8; READ_BUFFER];
    loop {
        match port.read(&mut buf) {
            // 读取超时（空闲）：检查设备是否已拔掉
            Ok(0) => {
                if closed.load(Ordering::SeqCst) {
                    return None;
                }
                if listed && !port.line_status_ok() {
                    match sys::list() {
                        Ok(ports) if !in_list(&ports, name) => {
                            return Some(WireEvent::Lost(
                                SerialError::Disconnected { port: name.into() }.to_string(),
                            ));
                        }
                        // 端口还在：驱动不支持查询线路状态，之后不再检查，只看读取是否出错
                        _ => listed = false,
                    }
                }
            }
            Ok(n) => {
                if events
                    .blocking_send(WireEvent::Data(buf[..n].to_vec()))
                    .is_err()
                {
                    return None;
                }
            }
            Err(_) if closed.load(Ordering::SeqCst) => return None,
            Err(e) => {
                return Some(WireEvent::Lost(format!("串口 {name} 已断开：{e}")));
            }
        }
    }
}

impl Wire for SerialWire {
    fn send(&self, data: &[u8]) {
        let _ = self.queue.lock().unwrap().send(data.to_vec());
    }

    /// 串口无从得知设备是否回显：本地回显为"自动"时不回显。
    fn remote_echo(&self) -> bool {
        true
    }

    fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // 中止进行中的读写；空的一块叫醒等待发送的写线程
        if let Some(port) = self.port.upgrade() {
            port.cancel();
        }
        let _ = self.queue.lock().unwrap().send(Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb(vid: &str, pid: &str, serial: &str) -> Option<UsbId> {
        Some(UsbId {
            vid: vid.into(),
            pid: pid.into(),
            serial_number: serial.into(),
        })
    }

    fn port(name: &str, usb: Option<UsbId>) -> PortInfo {
        PortInfo {
            name: name.into(),
            description: String::new(),
            usb,
        }
    }

    fn config(name: &str, usb: Option<UsbId>) -> SerialConfig {
        SerialConfig {
            port: name.into(),
            baud: 9600,
            data_bits: 8,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
            usb,
        }
    }

    #[test]
    fn resolve_finds_the_same_device() {
        let ports = [
            port("COM1", None),
            port("COM4", usb("1A86", "7523", "A1")),
            port("COM7", usb("0403", "6001", "FT1")),
            port("COM8", usb("0403", "6001", "FT2")),
        ];
        let cases = [
            ("端口号还在", config("com1", None), "COM1"),
            // 换了 USB 口：COM3 不在了，按序列号找到 COM4
            (
                "按序列号找回",
                config("COM3", usb("1a86", "7523", "A1")),
                "COM4",
            ),
            // 序列号优先于端口号：COM7 现在是另一个同型号的设备
            (
                "序列号优先",
                config("COM7", usb("0403", "6001", "FT2")),
                "COM8",
            ),
            // 没有序列号：唯一一个 VID / PID 相同的设备
            (
                "按 VID/PID 找回",
                config("COM3", usb("1A86", "7523", "")),
                "COM4",
            ),
            // VID / PID 相同的有多个：无法确定，按端口号打开
            (
                "多个同型号",
                config("COM3", usb("0403", "6001", "")),
                "COM3",
            ),
            ("列表中没有", config("COM9", None), "COM9"),
            // 没有 VID 的标识不参与匹配
            ("标识不完整", config("COM3", usb("", "", "")), "COM3"),
        ];
        for (name, config, want) in cases {
            assert_eq!(resolve(&config, &ports), want, "{name}");
        }
    }

    #[test]
    fn ports_sort_by_number() {
        let mut ports = vec![port("COM10", None), port("COM2", None), port("COM1", None)];
        sort_ports(&mut ports);
        let names: Vec<&str> = ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["COM1", "COM2", "COM10"]);
    }

    // 找不到端口可以重试（拔掉后自动重连，插回即恢复）
    #[test]
    fn opening_a_missing_port_is_retryable() {
        let name = if cfg!(windows) {
            "COM250"
        } else {
            "/dev/ttyTERMLETNOPE"
        };
        let error = test_open(&config(name, None)).unwrap_err();
        assert_eq!(error, SerialError::NotFound { port: name.into() });
        assert!(error.is_retryable());
        assert!(error.to_string().contains(name));
        assert!(
            !SerialError::Unsupported {
                port: name.into(),
                reason: String::new()
            }
            .is_retryable()
        );
    }

    // 本机的串口都有名称
    #[test]
    fn list_local_ports() {
        let ports = list_ports().unwrap();
        assert!(ports.iter().all(|p| !p.name.is_empty()), "{ports:?}");
        println!("本机串口：{ports:?}");
    }

    #[test]
    fn config_from_host() {
        let host = Host {
            protocol: crate::model::Protocol::Serial,
            address: "COM5".into(),
            serial: crate::model::SerialOptions {
                baud: 9600,
                flow_control: FlowControl::RtsCts,
                usb: usb("1A86", "7523", ""),
                ..Default::default()
            },
            ..Default::default()
        };
        let config = SerialConfig::for_host(&host);
        assert_eq!(config.port, "COM5");
        assert_eq!(config.baud, 9600);
        assert_eq!(config.data_bits, 8);
        assert_eq!(config.flow_control, FlowControl::RtsCts);
        assert_eq!(config.usb, usb("1A86", "7523", ""));
    }
}
