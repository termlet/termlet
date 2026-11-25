//! 同步的客户端：连接本机通道、握手、逐个发送请求。`termlet` 命令与第二个 Termlet 进程（把启动参数交给已有实例）用它。
//!
//! Windows 上打开管道时只给服务端"识别"级别的模拟权限（`SECURITY_IDENTIFICATION`），连接后核对管道的属主是当前用户：
//! 别的用户抢先创建了同名管道时拒绝连接，命令与本机路径不会交给它。

use std::io::{self, Write};

use crate::endpoint::Endpoint;
use crate::frame::{self, FrameError};
use crate::{ClientHello, ErrorBody, MAX_RESPONSE, Request, Response, Welcome, negotiate};

/// 连接失败的原因。
#[derive(Debug)]
pub enum ConnectError {
    /// 通道不存在：没有 Termlet 在运行（或命令行服务没有启动）
    NotRunning,
    /// 通道的属主不是当前用户（别的用户的进程占用了这个名字）
    Impostor,
    Io(io::Error),
    /// 服务端拒绝握手（协议版本不兼容等）
    Rejected(ErrorBody),
    /// 服务端的回答不符合协议
    Protocol(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NotRunning => f.write_str("Termlet 没有在运行"),
            ConnectError::Impostor => f.write_str("命令行通道不属于当前用户，已拒绝连接"),
            ConnectError::Io(e) => write!(f, "连接 Termlet 失败：{e}"),
            ConnectError::Rejected(body) => write!(f, "{}: {}", body.code, body.message),
            ConnectError::Protocol(e) => write!(f, "Termlet 的回答不符合协议：{e}"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// 请求失败（通信层面；业务错误是 [`Response::Error`]）。
#[derive(Debug)]
pub enum CallError {
    Io(io::Error),
    /// 服务端在回答前关闭了连接
    Closed,
    Protocol(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Io(e) => write!(f, "与 Termlet 通信失败：{e}"),
            CallError::Closed => f.write_str("Termlet 在回答前关闭了连接"),
            CallError::Protocol(e) => write!(f, "Termlet 的回答不符合协议：{e}"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<FrameError> for CallError {
    fn from(error: FrameError) -> Self {
        match error {
            FrameError::Closed => CallError::Closed,
            FrameError::Io(e) if frame::is_closed(&e) => CallError::Closed,
            FrameError::Io(e) => CallError::Io(e),
            other => CallError::Protocol(other.to_string()),
        }
    }
}

#[cfg(windows)]
type Stream = std::fs::File;
#[cfg(unix)]
type Stream = std::os::unix::net::UnixStream;

/// 一条已握手的连接。
pub struct Client {
    stream: Stream,
    welcome: Welcome,
    /// 连接还在（[`Abort`] 据此知道什么时候不用再取消）
    #[cfg(windows)]
    alive: std::sync::Arc<()>,
}

/// 从别的线程中止一条连接上进行中的请求（MCP 的 `notifications/cancelled`）：[`Client::call`] 随即返回错误，
/// 丢弃 [`Client`] 后服务端看到连接关闭，与 `termlet` 被结束时一样中止这次请求。
///
/// Unix 上关闭套接字的两个方向。Windows 上管道是同步打开的，用复制的句柄 `CancelIoEx` 取消正在等待的读写；
/// 取消时请求可能还没开始读（`CancelIoEx` 落空），因此每隔一小段时间再取消一次，直到 [`Client`] 被丢弃。
pub struct Abort {
    stream: Stream,
    #[cfg(windows)]
    alive: std::sync::Weak<()>,
}

impl Abort {
    /// 中止（不阻塞）。
    pub fn abort(self) {
        #[cfg(unix)]
        {
            let _ = self.stream.shutdown(std::net::Shutdown::Both);
        }
        #[cfg(windows)]
        {
            std::thread::spawn(move || windows::cancel_until_dropped(&self.stream, &self.alive));
        }
    }
}

impl std::fmt::Debug for Abort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Abort").finish_non_exhaustive()
    }
}

impl Client {
    /// 连接并握手。
    pub fn connect(endpoint: &Endpoint, hello: &ClientHello) -> Result<Self, ConnectError> {
        let mut stream = open(endpoint)?;
        frame::write(&mut stream, hello).map_err(ConnectError::Io)?;
        let response: Response = frame::read(&mut stream, MAX_RESPONSE).map_err(|e| match e {
            FrameError::Closed => ConnectError::Protocol("握手时连接被关闭".into()),
            FrameError::Io(e) => ConnectError::Io(e),
            other => ConnectError::Protocol(other.to_string()),
        })?;
        match response {
            Response::Welcome(welcome) if negotiate(welcome.protocol).is_some() => Ok(Self {
                stream,
                welcome,
                #[cfg(windows)]
                alive: std::sync::Arc::new(()),
            }),
            Response::Welcome(welcome) => Err(ConnectError::Rejected(ErrorBody::new(
                crate::ErrorCode::ProtocolMismatch,
                format!(
                    "Termlet {} 使用的协议版本 {} 太旧",
                    welcome.app_version, welcome.protocol
                ),
            ))),
            Response::Error(body) => Err(ConnectError::Rejected(body)),
            other => Err(ConnectError::Protocol(format!("握手时收到 {other:?}"))),
        }
    }

    /// 服务端的握手回答（协商后的版本、应用的版本）。
    pub fn welcome(&self) -> &Welcome {
        &self.welcome
    }

    /// 从别的线程中止这条连接的句柄（见 [`Abort`]）。
    pub fn abort_handle(&self) -> io::Result<Abort> {
        Ok(Abort {
            stream: self.stream.try_clone()?,
            #[cfg(windows)]
            alive: std::sync::Arc::downgrade(&self.alive),
        })
    }

    /// 发送一个请求并等待回答。`exec` 会一直等到远程命令结束；进程被结束时连接随之关闭，服务端中止这次请求。
    pub fn call(&mut self, request: &Request) -> Result<Response, CallError> {
        self.send(request)?;
        self.receive()
    }

    /// 对方是否支持批量执行（协商后的协议版本不低于 [`crate::BATCH_PROTOCOL_VERSION`]）。
    pub fn supports_batch(&self) -> bool {
        self.welcome.protocol >= crate::BATCH_PROTOCOL_VERSION
    }

    /// 只发送一个请求（流式返回多个响应的请求：之后用 [`Client::receive`] 逐个读）。
    pub fn send(&mut self, request: &Request) -> Result<(), CallError> {
        frame::write(&mut self.stream, request).map_err(|e| {
            if frame::is_closed(&e) {
                CallError::Closed
            } else {
                CallError::Io(e)
            }
        })
    }

    /// 读下一个响应。
    pub fn receive(&mut self) -> Result<Response, CallError> {
        Ok(frame::read(&mut self.stream, MAX_RESPONSE)?)
    }

    /// 直接写入字节（测试中模拟异常的客户端）。
    #[doc(hidden)]
    pub fn write_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.stream.write_all(bytes)
    }

    /// 直接读一帧（测试用）。
    #[doc(hidden)]
    pub fn read_response(&mut self) -> Result<Response, CallError> {
        self.receive()
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("welcome", &self.welcome)
            .finish_non_exhaustive()
    }
}

#[cfg(unix)]
fn open(endpoint: &Endpoint) -> Result<Stream, ConnectError> {
    match std::os::unix::net::UnixStream::connect(endpoint.socket_path()) {
        Ok(stream) => Ok(stream),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Err(ConnectError::NotRunning)
        }
        Err(e) => Err(ConnectError::Io(e)),
    }
}

#[cfg(windows)]
fn open(endpoint: &Endpoint) -> Result<Stream, ConnectError> {
    windows::open(endpoint.pipe_name())
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_BUSY, ERROR_SUCCESS, HANDLE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::SECURITY_IDENTIFICATION;
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{ConnectError, Stream};

    /// 所有实例都忙时最多等这么久。
    const BUSY_WAIT: Duration = Duration::from_secs(10);

    /// 反复取消 `file` 上进行中的同步读写，直到连接（`alive`）被丢弃；最多 10 秒（防止调用方一直不丢弃连接）。
    pub(super) fn cancel_until_dropped(file: &std::fs::File, alive: &std::sync::Weak<()>) {
        use windows_sys::Win32::System::IO::CancelIoEx;
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive.strong_count() > 0 && Instant::now() < deadline {
            // SAFETY: 句柄在 file 存活期间有效；不传 OVERLAPPED 表示取消这个文件对象上本进程发起的全部 I/O
            unsafe { CancelIoEx(file.as_raw_handle() as HANDLE, ptr::null()) };
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub(super) fn open(name: &str) -> Result<Stream, ConnectError> {
        let deadline = Instant::now() + BUSY_WAIT;
        let file = loop {
            let opened = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .security_qos_flags(SECURITY_IDENTIFICATION)
                .open(name);
            match opened {
                Ok(file) => break file,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                    if Instant::now() >= deadline {
                        return Err(ConnectError::Io(e));
                    }
                    let wide: Vec<u16> = OsStr::new(name).encode_wide().chain(Some(0)).collect();
                    // SAFETY: wide 是以 0 结尾的管道名
                    unsafe { WaitNamedPipeW(wide.as_ptr(), 1000) };
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Err(ConnectError::NotRunning);
                }
                Err(e) => return Err(ConnectError::Io(e)),
            }
        };
        match owned_by_current_user(file.as_raw_handle() as HANDLE) {
            Ok(true) => Ok(file),
            Ok(false) => Err(ConnectError::Impostor),
            Err(e) => Err(ConnectError::Io(e)),
        }
    }

    /// 对象的属主是否是当前用户。
    fn owned_by_current_user(handle: HANDLE) -> io::Result<bool> {
        let user = current_user()?;
        // SAFETY: GetSecurityInfo 写出属主（指向 sd 内部）与 sd，sd 用 LocalFree 释放；user 中是 TOKEN_USER
        unsafe {
            let mut owner: PSID = ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
            let err = GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut sd,
            );
            if err != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
            let user_sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
            let same = !owner.is_null() && EqualSid(owner, user_sid) != 0;
            LocalFree(sd);
            Ok(same)
        }
    }

    /// 当前进程的 TOKEN_USER（按 8 字节对齐的缓冲区）。
    fn current_user() -> io::Result<Vec<u64>> {
        // SAFETY: 标准的 Win32 调用序列；缓冲区大小由第一次调用返回
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0u32;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut size);
            let mut buffer = vec![0u64; (size as usize).div_ceil(8)];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buffer.as_mut_ptr().cast(),
                size,
                &mut size,
            );
            let error = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(error);
            }
            Ok(buffer)
        }
    }
}
