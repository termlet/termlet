//! Linux、macOS 的串口：termios 设置参数，`poll` 等数据（带超时，并可被 [`Port::cancel`] 叫醒）。
//!
//! 列出串口：Linux 读 `/sys/class/tty`（USB 串口沿设备目录向上找 `idVendor` 等），macOS 等列出 `/dev/cu.*`。

use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};

use super::{PortInfo, SerialConfig, SerialError};
use crate::model::{FlowControl, Parity, StopBits};

/// 单次读取最多等待的毫秒数：超时后检查设备是否还在。
const READ_TIMEOUT_MS: libc::c_int = 1000;

#[cfg(target_os = "linux")]
use libc::{TIOCEXCL, TIOCMGET};
// macOS 的 libc 没有导出这两个常量（与 BSD 的取值相同）
#[cfg(not(target_os = "linux"))]
const TIOCEXCL: libc::c_ulong = 0x2000_740d;
#[cfg(not(target_os = "linux"))]
const TIOCMGET: libc::c_ulong = 0x4004_746a;

/// 打开的串口。读只在一个线程、写只在另一个线程进行；[`Port::cancel`] 可以在任何线程调用。
pub(super) struct Port {
    fd: RawFd,
    /// 叫醒等待中的读写（关闭时）：读端在 `poll` 中，写端由 `cancel` 写入
    wake_read: RawFd,
    wake_write: RawFd,
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

impl Port {
    pub(super) fn open(name: &str, config: &SerialConfig) -> Result<Self, SerialError> {
        let port = name.to_string();
        let path = CString::new(name).map_err(|_| SerialError::NotFound { port: port.clone() })?;
        // SAFETY: 路径以 NUL 结尾；非阻塞打开，不让设备的载波检测卡住 open
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = last_error();
            return Err(match error.raw_os_error() {
                Some(libc::ENOENT) | Some(libc::ENXIO) | Some(libc::ENODEV) => {
                    SerialError::NotFound { port }
                }
                Some(libc::EBUSY) => SerialError::Busy { port },
                Some(libc::EACCES) | Some(libc::EPERM) => SerialError::PermissionDenied { port },
                _ => SerialError::Io {
                    port,
                    reason: error.to_string(),
                },
            });
        }
        let mut pipe = [0 as RawFd; 2];
        // SAFETY: pipe 是两个整数的数组
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            let reason = last_error().to_string();
            // SAFETY: fd 是刚打开的
            unsafe { libc::close(fd) };
            return Err(SerialError::Io { port, reason });
        }
        for end in pipe {
            // SAFETY: 两端都是刚创建的
            unsafe {
                libc::fcntl(end, libc::F_SETFD, libc::FD_CLOEXEC);
                libc::fcntl(end, libc::F_SETFL, libc::O_NONBLOCK);
            }
        }
        let port_handle = Self {
            fd,
            wake_read: pipe[0],
            wake_write: pipe[1],
        };
        // 独占：别的程序（同一用户的）不能再打开
        // SAFETY: fd 有效
        if unsafe { libc::ioctl(fd, TIOCEXCL as _) } != 0 {
            tracing::debug!("串口 {name} 不能设为独占：{}", last_error());
        }
        port_handle.configure(name, config)?;
        Ok(port_handle)
    }

    fn configure(&self, name: &str, config: &SerialConfig) -> Result<(), SerialError> {
        let io_error = |e: io::Error| SerialError::Io {
            port: name.to_string(),
            reason: e.to_string(),
        };
        let unsupported = |reason: &str| SerialError::Unsupported {
            port: name.to_string(),
            reason: reason.to_string(),
        };
        // SAFETY: termios 是普通的 C 结构，全零是合法的初值，随后由 tcgetattr 填写
        let mut tio: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd 有效，tio 可写
        if unsafe { libc::tcgetattr(self.fd, &mut tio) } != 0 {
            return Err(io_error(last_error()));
        }
        // SAFETY: tio 是 tcgetattr 填好的
        unsafe { libc::cfmakeraw(&mut tio) };
        tio.c_cflag |= libc::CLOCAL | libc::CREAD;
        tio.c_cflag &= !libc::CSIZE;
        tio.c_cflag |= match config.data_bits {
            5 => libc::CS5,
            6 => libc::CS6,
            7 => libc::CS7,
            8 => libc::CS8,
            _ => return Err(unsupported("数据位只能是 5 到 8")),
        };
        tio.c_cflag &= !(libc::PARENB | libc::PARODD);
        #[cfg(target_os = "linux")]
        {
            tio.c_cflag &= !libc::CMSPAR;
        }
        tio.c_iflag &= !(libc::INPCK | libc::IGNPAR);
        match config.parity {
            Parity::None => {}
            Parity::Odd => tio.c_cflag |= libc::PARENB | libc::PARODD,
            Parity::Even => tio.c_cflag |= libc::PARENB,
            #[cfg(target_os = "linux")]
            Parity::Mark => tio.c_cflag |= libc::PARENB | libc::PARODD | libc::CMSPAR,
            #[cfg(target_os = "linux")]
            Parity::Space => tio.c_cflag |= libc::PARENB | libc::CMSPAR,
            #[cfg(not(target_os = "linux"))]
            Parity::Mark | Parity::Space => {
                return Err(unsupported("这个系统不支持 mark、space 校验"));
            }
        }
        if config.parity != Parity::None {
            tio.c_iflag |= libc::INPCK;
        }
        match config.stop_bits {
            StopBits::One => tio.c_cflag &= !libc::CSTOPB,
            StopBits::Two => tio.c_cflag |= libc::CSTOPB,
            StopBits::OnePointFive => return Err(unsupported("这个系统不支持 1.5 个停止位")),
        }
        tio.c_cflag &= !libc::CRTSCTS;
        tio.c_iflag &= !(libc::IXON | libc::IXOFF | libc::IXANY);
        match config.flow_control {
            FlowControl::None => {}
            FlowControl::RtsCts => tio.c_cflag |= libc::CRTSCTS,
            FlowControl::XonXoff => tio.c_iflag |= libc::IXON | libc::IXOFF,
        }
        tio.c_cc[libc::VMIN] = 1;
        tio.c_cc[libc::VTIME] = 0;
        let speed = speed(config.baud).ok_or_else(|| unsupported("这个系统不支持这个波特率"))?;
        // SAFETY: tio 有效
        if unsafe { libc::cfsetispeed(&mut tio, speed) } != 0
            || unsafe { libc::cfsetospeed(&mut tio, speed) } != 0
        {
            return Err(unsupported("这个系统不支持这个波特率"));
        }
        // SAFETY: fd 有效，tio 是完整的设置
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &tio) } != 0 {
            let error = last_error();
            if error.raw_os_error() == Some(libc::EINVAL) {
                return Err(unsupported("驱动不接受这组参数"));
            }
            return Err(io_error(error));
        }
        Ok(())
    }

    /// 等 `fd` 可读（`readable`）或可写，同时看唤醒管道；超时返回 `Ok(false)`，被唤醒返回错误。
    fn wait(&self, readable: bool, timeout: libc::c_int) -> io::Result<bool> {
        let events = if readable {
            libc::POLLIN
        } else {
            libc::POLLOUT
        };
        let mut fds = [
            libc::pollfd {
                fd: self.fd,
                events,
                revents: 0,
            },
            libc::pollfd {
                fd: self.wake_read,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: fds 是两个有效的 pollfd
            let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
            if n < 0 {
                let error = last_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if fds[1].revents != 0 {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "串口已关闭"));
            }
            if n == 0 {
                return Ok(false);
            }
            // 有数据，或出错、挂断：交给 read / write 报告
            return Ok(true);
        }
    }

    /// 读取：有数据时返回读到的字节数，超时（空闲）时返回 0；设备拔掉、[`Port::cancel`] 之后返回错误。
    pub(super) fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if !self.wait(true, READ_TIMEOUT_MS)? {
            return Ok(0);
        }
        // SAFETY: 缓冲有效
        let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
        match n {
            n if n > 0 => Ok(n as usize),
            // 可读却读到结尾：设备已不在
            0 => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "设备已断开")),
            _ => {
                let error = last_error();
                match error.kind() {
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(0),
                    _ => Err(error),
                }
            }
        }
    }

    /// 写入全部数据（硬件流控时设备不允许发送就一直等，直到 [`Port::cancel`]）。
    pub(super) fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            // SAFETY: 数据有效
            let n = unsafe { libc::write(self.fd, data.as_ptr().cast(), data.len()) };
            if n > 0 {
                data = &data[n as usize..];
                continue;
            }
            let error = last_error();
            match error.kind() {
                io::ErrorKind::WouldBlock => {
                    self.wait(false, -1)?;
                }
                io::ErrorKind::Interrupted => {}
                _ => return Err(error),
            }
        }
        Ok(())
    }

    /// 查询线路状态是否成功：设备拔掉后查询失败。
    pub(super) fn line_status_ok(&self) -> bool {
        let mut status: libc::c_int = 0;
        // SAFETY: fd 有效，status 可写
        unsafe { libc::ioctl(self.fd, TIOCMGET as _, &mut status) == 0 }
    }

    /// 叫醒进行中的读写（关闭时）。
    pub(super) fn cancel(&self) {
        let byte = 1u8;
        // SAFETY: 管道的写端有效；满了也无妨（读写已经会被叫醒）
        unsafe { libc::write(self.wake_write, (&byte as *const u8).cast(), 1) };
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: 三个描述符都是 open 打开的，只关闭一次；读写线程都已放下端口
        unsafe {
            libc::close(self.fd);
            libc::close(self.wake_read);
            libc::close(self.wake_write);
        }
    }
}

/// 波特率对应的 termios 速度。Linux 只认这些固定的值；BSD、macOS 的速度就是波特率本身。
#[cfg(target_os = "linux")]
fn speed(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        50 => libc::B50,
        75 => libc::B75,
        110 => libc::B110,
        134 => libc::B134,
        150 => libc::B150,
        200 => libc::B200,
        300 => libc::B300,
        600 => libc::B600,
        1200 => libc::B1200,
        1800 => libc::B1800,
        2400 => libc::B2400,
        4800 => libc::B4800,
        9600 => libc::B9600,
        19200 => libc::B19200,
        38400 => libc::B38400,
        57600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        460_800 => libc::B460800,
        500_000 => libc::B500000,
        576_000 => libc::B576000,
        921_600 => libc::B921600,
        1_000_000 => libc::B1000000,
        1_152_000 => libc::B1152000,
        1_500_000 => libc::B1500000,
        2_000_000 => libc::B2000000,
        2_500_000 => libc::B2500000,
        3_000_000 => libc::B3000000,
        3_500_000 => libc::B3500000,
        4_000_000 => libc::B4000000,
        _ => return None,
    })
}

#[cfg(not(target_os = "linux"))]
fn speed(baud: u32) -> Option<libc::speed_t> {
    Some(baud as libc::speed_t)
}

/// 列出串口。
pub(super) fn list() -> Result<Vec<PortInfo>, SerialError> {
    #[cfg(target_os = "linux")]
    {
        list_linux(Path::new("/sys/class/tty"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        list_dev(Path::new("/dev"))
    }
}

/// Linux：`/sys/class/tty` 中有设备（`device` 链接）的终端。主板上的 `ttyS*` 多数只是占位（驱动登记了、
/// 并没有硬件），其 `type` 为 0（PORT_UNKNOWN）的不列出。
#[cfg(target_os = "linux")]
fn list_linux(sys_tty: &Path) -> Result<Vec<PortInfo>, SerialError> {
    let entries = std::fs::read_dir(sys_tty).map_err(|e| SerialError::List(e.to_string()))?;
    let mut ports = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let dir = entry.path();
        let Ok(device) = std::fs::canonicalize(dir.join("device")) else {
            continue;
        };
        if read_trimmed(&dir.join("type")).as_deref() == Some("0") {
            continue;
        }
        let usb = usb_ancestor(&device);
        let description = usb
            .as_ref()
            .and_then(|usb_dir| read_trimmed(&usb_dir.join("product")))
            .unwrap_or_default();
        ports.push(PortInfo {
            name: format!("/dev/{name}"),
            description,
            usb: usb.and_then(|usb_dir| {
                Some(crate::model::UsbId {
                    vid: read_trimmed(&usb_dir.join("idVendor"))?.to_ascii_uppercase(),
                    pid: read_trimmed(&usb_dir.join("idProduct"))?.to_ascii_uppercase(),
                    serial_number: read_trimmed(&usb_dir.join("serial")).unwrap_or_default(),
                })
            }),
        });
    }
    Ok(ports)
}

/// 沿设备目录向上找 USB 设备（有 `idVendor` 的目录）。
#[cfg(target_os = "linux")]
fn usb_ancestor(device: &Path) -> Option<PathBuf> {
    device
        .ancestors()
        .take(8)
        .find(|dir| dir.join("idVendor").is_file())
        .map(Path::to_path_buf)
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// macOS、BSD：`/dev/cu.*`（macOS，拨出用的设备，打开时不等载波）、`/dev/cuaU*`（FreeBSD 的 USB 串口）。
#[cfg(not(target_os = "linux"))]
fn list_dev(dev: &Path) -> Result<Vec<PortInfo>, SerialError> {
    let entries = std::fs::read_dir(dev).map_err(|e| SerialError::List(e.to_string()))?;
    let mut ports: Vec<PortInfo> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("cu.") || name.starts_with("cuaU"))
        .map(|name| PortInfo {
            name: PathBuf::from(dev).join(name).to_string_lossy().into_owned(),
            ..Default::default()
        })
        .collect();
    ports.dedup();
    Ok(ports)
}

// 用伪终端模拟串口设备
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::ffi::CStr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;

    /// 新的伪终端：主端（模拟设备）与从端的路径（当作串口打开）。
    fn pty() -> (RawFd, String) {
        let (mut master, mut slave) = (0, 0);
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: 输出参数都有效，名称缓冲足够长
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                name.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        assert_eq!(rc, 0, "无法创建伪终端");
        // SAFETY: openpty 写入了以 NUL 结尾的名称
        let path = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_str()
            .unwrap()
            .to_string();
        // SAFETY: 从端由测试按路径重新打开
        unsafe { libc::close(slave) };
        (master, path)
    }

    fn config(port: &str) -> SerialConfig {
        SerialConfig {
            port: port.into(),
            baud: 115_200,
            data_bits: 8,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::XonXoff,
            usb: None,
        }
    }

    // 收发数据、读取超时不误判为断开、设备一端关闭（拔掉）后读取出错
    #[test]
    fn pty_as_a_serial_device() {
        let (device, path) = pty();
        let port = Port::open(&path, &config(&path)).unwrap();
        port.write_all(b"AT\r").unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < 3 {
            // SAFETY: 缓冲有效
            let n = unsafe { libc::read(device, buf.as_mut_ptr().cast(), buf.len()) };
            assert!(n > 0);
            got.extend_from_slice(&buf[..n as usize]);
        }
        assert_eq!(got, b"AT\r");

        let started = Instant::now();
        assert_eq!(port.read(&mut buf).unwrap(), 0, "空闲时超时返回 0");
        assert!(started.elapsed() >= Duration::from_millis(900));
        // SAFETY: 数据有效
        unsafe { libc::write(device, b"OK\r\n".as_ptr().cast(), 4) };
        let mut out = Vec::new();
        while !out.ends_with(b"OK\r\n") {
            let n = port.read(&mut buf).unwrap();
            out.extend_from_slice(&buf[..n]);
        }

        // SAFETY: 主端只关闭一次
        unsafe { libc::close(device) };
        let deadline = Instant::now() + Duration::from_secs(5);
        while port.read(&mut buf).is_ok() {
            assert!(Instant::now() < deadline, "设备关闭后读取没有出错");
        }
    }

    // 关闭时叫醒等待中的读取；打不开的端口与参数
    #[test]
    fn cancel_and_errors() {
        let (device, path) = pty();
        let port = Arc::new(Port::open(&path, &config(&path)).unwrap());
        let reader = port.clone();
        let started = Instant::now();
        let waiting = std::thread::spawn(move || reader.read(&mut [0u8; 8]));
        std::thread::sleep(Duration::from_millis(100));
        port.cancel();
        assert!(waiting.join().unwrap().is_err());
        assert!(started.elapsed() < Duration::from_millis(900));
        // SAFETY: 主端只关闭一次
        unsafe { libc::close(device) };

        assert!(matches!(
            Port::open("/dev/ttyTERMLETNOPE", &config("x")),
            Err(SerialError::NotFound { .. })
        ));
        // 独占打开过的终端在设备一端关闭之前不能再打开：每次用新的伪终端
        let bad_settings: [fn(&mut SerialConfig); 2] = [
            |c| c.stop_bits = StopBits::OnePointFive,
            |c| c.baud = 12_345,
        ];
        for change in bad_settings {
            let (device, path) = pty();
            let mut bad = config(&path);
            change(&mut bad);
            assert!(matches!(
                Port::open(&path, &bad),
                Err(SerialError::Unsupported { .. })
            ));
            // SAFETY: 主端只关闭一次
            unsafe { libc::close(device) };
        }
        // 已经打开的端口：被占用
        let (device, path) = pty();
        let first = Port::open(&path, &config(&path)).unwrap();
        assert!(matches!(
            Port::open(&path, &config(&path)),
            Err(SerialError::Busy { .. })
        ));
        drop(first);
        // SAFETY: 主端只关闭一次
        unsafe { libc::close(device) };
        // 伪终端不是串口设备：不在列表中
        let listed = list().unwrap();
        assert!(listed.iter().all(|p| p.name != path), "{listed:?}");
    }
}
