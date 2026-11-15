//! Windows 的串口：通信 API（`SetCommState`、重叠 I/O）与 SetupAPI（列出端口与 USB 标识）。
//!
//! 句柄以重叠方式打开：同步句柄上的读写由系统依次进行，读取等待数据时写入也要等，按键回显会慢上一个读取超时。
//! 读取的超时设为"有数据立即返回，没有时最多等 [`READ_TIMEOUT_MS`] 毫秒"，超时后由调用方检查设备是否还在。

use std::io;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Devices::Communication::{
    COMMTIMEOUTS, DCB, EVENPARITY, GetCommModemStatus, GetCommState, MARKPARITY, NOPARITY,
    ODDPARITY, ONE5STOPBITS, ONESTOPBIT, SPACEPARITY, SetCommState, SetCommTimeouts, SetupComm,
    TWOSTOPBITS,
};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CR_SUCCESS, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DEV,
    GUID_DEVCLASS_PORTS, HDEVINFO, SP_DEVINFO_DATA, SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER,
    ERROR_IO_PENDING, ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, GENERIC_READ, GENERIC_WRITE,
    GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Registry::KEY_READ;
use windows_sys::Win32::System::Threading::CreateEventW;

use super::{PortInfo, SerialConfig, SerialError};
use crate::model::{FlowControl, Parity, StopBits, UsbId};

/// 单次读取最多等待的毫秒数：超时后检查设备是否还在。
const READ_TIMEOUT_MS: u32 = 1000;
/// 驱动的收发缓冲（建议值，驱动可以不接受）。
const QUEUE_SIZE: u32 = 64 * 1024;

// DCB 中的位（`_bitfield`）
const F_BINARY: u32 = 1 << 0;
const F_PARITY: u32 = 1 << 1;
const F_OUTX_CTS_FLOW: u32 = 1 << 2;
const F_OUTX_DSR_FLOW: u32 = 1 << 3;
const F_DTR_CONTROL_SHIFT: u32 = 4;
const F_DSR_SENSITIVITY: u32 = 1 << 6;
const F_TX_CONTINUE_ON_XOFF: u32 = 1 << 7;
const F_OUTX: u32 = 1 << 8;
const F_INX: u32 = 1 << 9;
const F_ERROR_CHAR: u32 = 1 << 10;
const F_NULL: u32 = 1 << 11;
const F_RTS_CONTROL_SHIFT: u32 = 12;
const F_ABORT_ON_ERROR: u32 = 1 << 14;
const CONTROL_MASK: u32 = 0b11;
const DTR_CONTROL_ENABLE: u32 = 1;
const RTS_CONTROL_ENABLE: u32 = 1;
const RTS_CONTROL_HANDSHAKE: u32 = 2;

/// 打开的串口。读只在一个线程、写只在另一个线程进行；[`Port::cancel`] 可以在任何线程调用。
pub(super) struct Port {
    handle: HANDLE,
}

// 句柄可以在线程间传递与共用：读写各用自己的 OVERLAPPED 与事件
unsafe impl Send for Port {}
unsafe impl Sync for Port {}

/// `COM10` 以上要写成设备路径 `\\.\COM10`；已经是设备路径的原样使用。
fn device_path(name: &str) -> String {
    if name.starts_with(r"\\.\") || name.starts_with(r"\\?\") {
        name.to_string()
    } else {
        format!(r"\\.\{name}")
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

impl Port {
    pub(super) fn open(name: &str, config: &SerialConfig) -> Result<Self, SerialError> {
        let path = wide(&device_path(name));
        // SAFETY: 路径以 NUL 结尾；其余参数是常量
        let handle = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: 紧接着失败的调用读取错误码
            let code = unsafe { GetLastError() };
            let port = name.to_string();
            return Err(match code {
                ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND => SerialError::NotFound { port },
                // 串口是独占的：被别的程序打开时报"拒绝访问"
                ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION => SerialError::Busy { port },
                _ => SerialError::Io {
                    port,
                    reason: io::Error::from_raw_os_error(code as i32).to_string(),
                },
            });
        }
        let port = Self { handle };
        port.configure(name, config)?;
        Ok(port)
    }

    fn configure(&self, name: &str, config: &SerialConfig) -> Result<(), SerialError> {
        let io_error = |e: io::Error| SerialError::Io {
            port: name.to_string(),
            reason: e.to_string(),
        };
        // SAFETY: 句柄有效；缓冲大小只是建议值，驱动不接受时忽略
        unsafe { SetupComm(self.handle, QUEUE_SIZE, QUEUE_SIZE) };
        let mut dcb = DCB {
            DCBlength: std::mem::size_of::<DCB>() as u32,
            ..Default::default()
        };
        // SAFETY: dcb 是有效的可写结构
        if unsafe { GetCommState(self.handle, &mut dcb) } == 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        apply_settings(&mut dcb, config).map_err(|reason| SerialError::Unsupported {
            port: name.to_string(),
            reason: reason.to_string(),
        })?;
        // SAFETY: dcb 是有效的结构
        if unsafe { SetCommState(self.handle, &dcb) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Err(SerialError::Unsupported {
                    port: name.to_string(),
                    reason: describe(config),
                });
            }
            return Err(io_error(error));
        }
        let timeouts = COMMTIMEOUTS {
            // 有数据就立即返回；没有时等到第一个字节或超时
            ReadIntervalTimeout: u32::MAX,
            ReadTotalTimeoutMultiplier: u32::MAX,
            ReadTotalTimeoutConstant: READ_TIMEOUT_MS,
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 0,
        };
        // SAFETY: 句柄有效
        if unsafe { SetCommTimeouts(self.handle, &timeouts) } == 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        Ok(())
    }

    /// 读取：有数据时返回读到的字节数，超时（空闲）时返回 0；[`Port::cancel`] 之后返回错误。
    pub(super) fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_mut_ptr();
        // SAFETY: 缓冲在等到操作完成之前一直有效
        self.overlapped(|handle, overlapped| unsafe {
            ReadFile(handle, ptr, len, null_mut(), overlapped)
        })
    }

    /// 写入全部数据（硬件流控时设备不允许发送就一直等，直到 [`Port::cancel`]）。
    pub(super) fn write_all(&self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            let len = data.len().min(u32::MAX as usize) as u32;
            let ptr = data.as_ptr();
            // SAFETY: 数据在等到操作完成之前一直有效
            let written = self.overlapped(|handle, overlapped| unsafe {
                WriteFile(handle, ptr, len, null_mut(), overlapped)
            })?;
            if written == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "串口没有写入数据"));
            }
            data = &data[written..];
        }
        Ok(())
    }

    /// 发起一个重叠操作并等它完成（无论同步完成还是挂起），返回传输的字节数。
    fn overlapped(&self, start: impl FnOnce(HANDLE, *mut OVERLAPPED) -> i32) -> io::Result<usize> {
        // SAFETY: 手动重置、初始未触发的匿名事件
        let event = unsafe { CreateEventW(null(), 1, 0, null()) };
        if event.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let started = start(self.handle, &mut overlapped);
        let result = if started == 0
            // SAFETY: 紧接着失败的调用读取错误码
            && unsafe { GetLastError() } != ERROR_IO_PENDING
        {
            Err(io::Error::last_os_error())
        } else {
            let mut transferred = 0u32;
            // SAFETY: 等到操作完成（含被取消）才返回，此后 overlapped 与缓冲不再被系统使用
            if unsafe { GetOverlappedResult(self.handle, &overlapped, &mut transferred, 1) } == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(transferred as usize)
            }
        };
        // SAFETY: 事件是这里创建的，操作已经结束
        unsafe { CloseHandle(event) };
        result
    }

    /// 查询线路状态是否成功：设备拔掉后多数驱动查询失败。
    pub(super) fn line_status_ok(&self) -> bool {
        let mut status = 0u32;
        // SAFETY: 句柄有效
        unsafe { GetCommModemStatus(self.handle, &mut status) != 0 }
    }

    /// 中止这个端口上进行中的读写（关闭时）。
    pub(super) fn cancel(&self) {
        // SAFETY: 句柄有效；没有进行中的操作时调用也无妨
        unsafe { CancelIoEx(self.handle, null()) };
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: 句柄是 open 打开的，只关闭一次；读写线程都已放下端口
        unsafe { CloseHandle(self.handle) };
    }
}

/// 把参数写进 DCB：二进制模式；不流控时 DTR、RTS 保持有效（与 go.bug.st/serial 相同）。
fn apply_settings(dcb: &mut DCB, config: &SerialConfig) -> Result<(), &'static str> {
    dcb.BaudRate = config.baud;
    dcb.ByteSize = config.data_bits;
    dcb.Parity = match config.parity {
        Parity::None => NOPARITY,
        Parity::Odd => ODDPARITY,
        Parity::Even => EVENPARITY,
        Parity::Mark => MARKPARITY,
        Parity::Space => SPACEPARITY,
    };
    dcb.StopBits = match config.stop_bits {
        StopBits::One => ONESTOPBIT,
        StopBits::OnePointFive => ONE5STOPBITS,
        StopBits::Two => TWOSTOPBITS,
    };
    let mut flags = dcb._bitfield;
    flags &= !(F_PARITY
        | F_OUTX_CTS_FLOW
        | F_OUTX_DSR_FLOW
        | (CONTROL_MASK << F_DTR_CONTROL_SHIFT)
        | F_DSR_SENSITIVITY
        | F_OUTX
        | F_INX
        | F_ERROR_CHAR
        | F_NULL
        | (CONTROL_MASK << F_RTS_CONTROL_SHIFT)
        | F_ABORT_ON_ERROR);
    flags |= F_BINARY | F_TX_CONTINUE_ON_XOFF | (DTR_CONTROL_ENABLE << F_DTR_CONTROL_SHIFT);
    if config.parity != Parity::None {
        flags |= F_PARITY;
    }
    match config.flow_control {
        FlowControl::None => flags |= RTS_CONTROL_ENABLE << F_RTS_CONTROL_SHIFT,
        FlowControl::RtsCts => {
            flags |= F_OUTX_CTS_FLOW | (RTS_CONTROL_HANDSHAKE << F_RTS_CONTROL_SHIFT)
        }
        FlowControl::XonXoff => {
            flags |= F_OUTX | F_INX | (RTS_CONTROL_ENABLE << F_RTS_CONTROL_SHIFT);
            dcb.XonChar = 0x11;
            dcb.XoffChar = 0x13;
            dcb.XonLim = 2048;
            dcb.XoffLim = 512;
        }
    }
    dcb._bitfield = flags;
    if !(5..=8).contains(&config.data_bits) {
        return Err("数据位只能是 5 到 8");
    }
    Ok(())
}

/// 参数的说明（报错时）。
fn describe(config: &SerialConfig) -> String {
    format!(
        "{} 波特、{} 数据位、{:?} 校验、{:?} 停止位",
        config.baud, config.data_bits, config.parity, config.stop_bits
    )
}

/// 列出串口：设备管理器"端口（COM 和 LPT）"中现有的 COM 口，带名称与 USB 标识；
/// 再补上注册表 `HARDWARE\DEVICEMAP\SERIALCOMM` 中有、设备管理器中没有的（部分虚拟串口）。
pub(super) fn list() -> Result<Vec<PortInfo>, SerialError> {
    let mut ports = list_devices()?;
    if let Ok(key) = windows_registry::LOCAL_MACHINE.open(r"HARDWARE\DEVICEMAP\SERIALCOMM")
        && let Ok(values) = key.values()
    {
        for (_, value) in values {
            let Ok(name) = String::try_from(value) else {
                continue;
            };
            let name = name.trim().to_string();
            if !name.is_empty() && !ports.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
                ports.push(PortInfo {
                    name,
                    ..Default::default()
                });
            }
        }
    }
    Ok(ports)
}

/// 设备信息集合，用完销毁。
struct DeviceSet(HDEVINFO);

impl Drop for DeviceSet {
    fn drop(&mut self) {
        // SAFETY: 集合是 SetupDiGetClassDevsW 返回的，只销毁一次
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn list_devices() -> Result<Vec<PortInfo>, SerialError> {
    // SAFETY: GUID 是常量；只列出现在插着的设备
    let set =
        unsafe { SetupDiGetClassDevsW(&GUID_DEVCLASS_PORTS, null(), null_mut(), DIGCF_PRESENT) };
    if set == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(SerialError::List(io::Error::last_os_error().to_string()));
    }
    let set = DeviceSet(set);
    let mut ports = Vec::new();
    for index in 0.. {
        let mut info = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        // SAFETY: info 是有效的可写结构；没有更多设备时返回失败
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut info) } == 0 {
            break;
        }
        // 只要 COM 口（这个类中还有打印机的 LPT 口）
        let Some(name) =
            port_name(&set, &info).filter(|n| n.to_ascii_uppercase().starts_with("COM"))
        else {
            continue;
        };
        let description = device_property(&set, &info, SPDRP_FRIENDLYNAME)
            .or_else(|| device_property(&set, &info, SPDRP_DEVICEDESC))
            .unwrap_or_default();
        let instance = instance_id(&set, &info).unwrap_or_default();
        let mut usb = parse_device_id(&instance);
        // 复合设备（`&MI_xx` 的接口）的序列号在上级设备的标识中
        if usb.as_ref().is_some_and(|u| u.serial_number.is_empty())
            && instance.to_ascii_uppercase().contains("&MI_")
            && let Some(parent) = parent_id(info.DevInst)
            && let Some(from_parent) = parse_device_id(&parent)
            && !from_parent.serial_number.is_empty()
        {
            usb = Some(from_parent);
        }
        ports.push(PortInfo {
            name,
            description,
            usb,
        });
    }
    Ok(ports)
}

/// 设备的 `PortName`（在设备的注册表键中）。
fn port_name(set: &DeviceSet, info: &SP_DEVINFO_DATA) -> Option<String> {
    // SAFETY: 集合与设备信息有效；失败时返回 INVALID_HANDLE_VALUE
    let key =
        unsafe { SetupDiOpenDevRegKey(set.0, info, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ) };
    if key.is_null() || key == INVALID_HANDLE_VALUE {
        return None;
    }
    // SAFETY: 键是刚打开的，交给 Key 在丢弃时关闭
    let key = unsafe { windows_registry::Key::from_raw(key) };
    key.get_string("PortName")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// 设备的字符串属性（友好名称、描述）。
fn device_property(set: &DeviceSet, info: &SP_DEVINFO_DATA, property: u32) -> Option<String> {
    let mut buffer = vec![0u16; 512];
    let mut kind = 0u32;
    // SAFETY: 缓冲大小按字节传入
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set.0,
            info,
            property,
            &mut kind,
            buffer.as_mut_ptr().cast(),
            (buffer.len() * 2) as u32,
            null_mut(),
        )
    };
    (ok != 0)
        .then(|| from_wide(&buffer))
        .filter(|s| !s.is_empty())
}

/// 设备的实例标识（`USB\VID_1A86&PID_7523\5&2A6B1C3&0&2`）。
fn instance_id(set: &DeviceSet, info: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buffer = vec![0u16; 512];
    // SAFETY: 缓冲大小按字符传入
    let ok = unsafe {
        SetupDiGetDeviceInstanceIdW(
            set.0,
            info,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            null_mut(),
        )
    };
    (ok != 0).then(|| from_wide(&buffer))
}

/// 上级设备的实例标识。
fn parent_id(device: u32) -> Option<String> {
    let mut parent = 0u32;
    // SAFETY: 输出参数有效
    if unsafe { CM_Get_Parent(&mut parent, device, 0) } != CR_SUCCESS {
        return None;
    }
    let mut buffer = vec![0u16; 512];
    // SAFETY: 缓冲大小按字符传入
    if unsafe { CM_Get_Device_IDW(parent, buffer.as_mut_ptr(), buffer.len() as u32, 0) }
        != CR_SUCCESS
    {
        return None;
    }
    Some(from_wide(&buffer))
}

fn from_wide(buffer: &[u16]) -> String {
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).trim().to_string()
}

/// 从实例标识中取出 USB 标识（与 go.bug.st/serial 的规则相同）：
/// - Windows 自带的 USB 驱动：`USB\VID_1A86&PID_7523\序列号`，最后一段只含字母数字时是设备的序列号，
///   含 `&` 的是系统生成的位置标识，不是序列号；
/// - FTDI 的驱动：`FTDIBUS\VID_0403+PID_6001+序列号A\0000`。
fn parse_device_id(id: &str) -> Option<UsbId> {
    let upper = id.to_ascii_uppercase();
    let hex4 = |s: &str| {
        let s = s.get(..4)?;
        s.bytes()
            .all(|b| b.is_ascii_hexdigit())
            .then(|| s.to_string())
    };
    let word = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if upper.starts_with("USB") {
        let vid_at = upper.find("VID_")? + 4;
        let vid = hex4(&id[vid_at..])?;
        let pid_at = upper[vid_at..].find("&PID_")? + vid_at + 5;
        let pid = hex4(&id[pid_at..])?;
        let rest = &id[pid_at + 4..];
        // 接口号（&MI_00）等之后的第一个反斜杠后是序列号或位置标识
        let serial = rest
            .find('\\')
            .map(|i| &rest[i + 1..])
            .filter(|s| word(s))
            .unwrap_or_default();
        return Some(UsbId {
            vid,
            pid,
            serial_number: serial.to_string(),
        });
    }
    if upper.starts_with("FTDIBUS") {
        let vid_at = upper.find("VID_")? + 4;
        let vid = hex4(&id[vid_at..])?;
        let pid_at = upper[vid_at..].find("+PID_")? + vid_at + 5;
        let pid = hex4(&id[pid_at..])?;
        let rest = &id[pid_at + 4..];
        let serial = rest
            .strip_prefix('+')
            .map(|s| s.split('\\').next().unwrap_or_default())
            .filter(|s| word(s))
            .unwrap_or_default();
        return Some(UsbId {
            vid,
            pid,
            serial_number: serial.to_string(),
        });
    }
    None
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

    #[test]
    fn usb_ids_from_instance_ids() {
        // CH340：没有序列号，最后一段是系统生成的位置标识
        assert_eq!(
            parse_device_id(r"USB\VID_1A86&PID_7523\5&2A6B1C3&0&2"),
            usb("1A86", "7523", "")
        );
        // CP210x、Arduino：带序列号
        assert_eq!(
            parse_device_id(r"USB\VID_10C4&PID_EA60\0001"),
            usb("10C4", "EA60", "0001")
        );
        assert_eq!(
            parse_device_id(r"USB\VID_2341&PID_0043\85736323838351F0E1E1"),
            usb("2341", "0043", "85736323838351F0E1E1")
        );
        // 复合设备的接口：序列号要从上级设备取
        assert_eq!(
            parse_device_id(r"USB\VID_2E8A&PID_000A&MI_00\6&1B4D0F2&0&0000"),
            usb("2E8A", "000A", "")
        );
        // FTDI 的驱动
        assert_eq!(
            parse_device_id(r"FTDIBUS\VID_0403+PID_6001+A50285BIA\0000"),
            usb("0403", "6001", "A50285BIA")
        );
        assert_eq!(
            parse_device_id(r"FTDIBUS\VID_0403+PID_6001\0000"),
            usb("0403", "6001", "")
        );
        // 主板上的串口、蓝牙、不完整的标识
        assert_eq!(parse_device_id(r"ACPI\PNP0501\0"), None);
        assert_eq!(
            parse_device_id(r"BTHENUM\{00001101-0000}_LOCALMFG&0002"),
            None
        );
        assert_eq!(parse_device_id(r"USB\VID_12"), None);
        assert_eq!(parse_device_id(""), None);
    }

    #[test]
    fn device_paths() {
        assert_eq!(device_path("COM3"), r"\\.\COM3");
        assert_eq!(device_path(r"\\.\COM10"), r"\\.\COM10");
    }

    #[test]
    fn dcb_settings() {
        let mut config = SerialConfig {
            port: "COM1".into(),
            baud: 9600,
            data_bits: 7,
            parity: Parity::Even,
            stop_bits: StopBits::Two,
            flow_control: FlowControl::None,
            usb: None,
        };
        let mut dcb = DCB {
            // 驱动原来的设置：硬件流控、出错时中止
            _bitfield: F_OUTX_CTS_FLOW | F_ABORT_ON_ERROR | (2 << F_RTS_CONTROL_SHIFT),
            ..Default::default()
        };
        apply_settings(&mut dcb, &config).unwrap();
        assert_eq!(dcb.BaudRate, 9600);
        assert_eq!(dcb.ByteSize, 7);
        assert_eq!(dcb.Parity, EVENPARITY);
        assert_eq!(dcb.StopBits, TWOSTOPBITS);
        let flags = dcb._bitfield;
        assert_ne!(flags & F_BINARY, 0);
        assert_ne!(flags & F_PARITY, 0);
        assert_eq!(
            flags & (F_OUTX_CTS_FLOW | F_ABORT_ON_ERROR | F_OUTX | F_INX),
            0
        );
        assert_eq!(
            (flags >> F_DTR_CONTROL_SHIFT) & CONTROL_MASK,
            DTR_CONTROL_ENABLE
        );
        assert_eq!(
            (flags >> F_RTS_CONTROL_SHIFT) & CONTROL_MASK,
            RTS_CONTROL_ENABLE
        );

        config.flow_control = FlowControl::RtsCts;
        config.parity = Parity::None;
        apply_settings(&mut dcb, &config).unwrap();
        let flags = dcb._bitfield;
        assert_eq!(flags & F_PARITY, 0);
        assert_ne!(flags & F_OUTX_CTS_FLOW, 0);
        assert_eq!(
            (flags >> F_RTS_CONTROL_SHIFT) & CONTROL_MASK,
            RTS_CONTROL_HANDSHAKE
        );

        config.flow_control = FlowControl::XonXoff;
        apply_settings(&mut dcb, &config).unwrap();
        let flags = dcb._bitfield;
        assert_ne!(flags & F_OUTX, 0);
        assert_ne!(flags & F_INX, 0);
        assert_eq!(flags & F_OUTX_CTS_FLOW, 0);
        assert_eq!((dcb.XonChar, dcb.XoffChar), (0x11, 0x13));

        config.data_bits = 9;
        assert!(apply_settings(&mut dcb, &config).is_err());
    }
}
