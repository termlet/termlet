//! 监听本机通道：Windows 命名管道（只允许当前用户与 SYSTEM，属主为当前用户，拒绝远程客户端，只建第一个实例），
//! Unix 套接字（`run/` 为 0700，套接字本身 0600）。

use tokio::io::{AsyncRead, AsyncWrite};

use termlet_proto::endpoint::Endpoint;

/// 一条连接。
pub(super) trait Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Conn for T {}

pub(super) use imp::Listener;

#[cfg(windows)]
mod imp {
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use tokio::net::windows::named_pipe::{NamedPipeServer, PipeMode, ServerOptions};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::{Conn, Endpoint};

    /// 管道的安全描述符：属主为当前用户，受保护的 DACL 只允许当前用户与 SYSTEM。
    struct Security {
        descriptor: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: 描述符是 LocalAlloc 分配的一块只读内存，创建管道时只读取它
    unsafe impl Send for Security {}
    unsafe impl Sync for Security {}

    impl Security {
        fn current_user_only() -> io::Result<Self> {
            let sid = current_user_sid()?;
            let sddl = format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GA;;;SY)");
            let wide: Vec<u16> = OsStr::new(&sddl).encode_wide().chain(Some(0)).collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
            // SAFETY: wide 以 0 结尾；成功时 descriptor 由 LocalFree 释放（见 Drop）
            let ok = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    wide.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { descriptor })
        }

        fn attributes(&self) -> SECURITY_ATTRIBUTES {
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: self.descriptor,
                bInheritHandle: 0,
            }
        }
    }

    impl Drop for Security {
        fn drop(&mut self) {
            // SAFETY: descriptor 来自 ConvertStringSecurityDescriptorToSecurityDescriptorW
            unsafe { LocalFree(self.descriptor) };
        }
    }

    /// 当前用户的 SID（`S-1-5-21-…`）。
    pub(super) fn current_user_sid() -> io::Result<String> {
        // SAFETY: 标准的 Win32 调用序列；缓冲区大小由第一次调用返回，按 8 字节对齐；字符串由 LocalFree 释放
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
            let sid = (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut text = ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(value)
        }
    }

    pub(in crate::cli) struct Listener {
        name: String,
        security: Security,
        next: NamedPipeServer,
    }

    impl Listener {
        /// 开始监听（要在核心的运行时中调用）。同名管道已存在（另一个进程占用了这个名字）时失败。
        pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let security = Security::current_user_only()?;
            let name = endpoint.pipe_name().to_string();
            let next = create(&name, &security, true)?;
            Ok(Self {
                name,
                security,
                next,
            })
        }

        /// 等下一条连接。
        pub async fn accept(&mut self) -> io::Result<Box<dyn Conn>> {
            let result = self.next.connect().await;
            // 先建好下一个实例再交出这一个：连接之间没有"管道不存在"的空档
            let next = create(&self.name, &self.security, false)?;
            let connected = std::mem::replace(&mut self.next, next);
            result?;
            Ok(Box::new(connected))
        }
    }

    fn create(name: &str, security: &Security, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = security.attributes();
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .pipe_mode(PipeMode::Byte)
            .access_inbound(true)
            .access_outbound(true);
        // SAFETY: attributes 与其中的描述符在调用期间有效
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use tokio::net::UnixListener;

    use super::{Conn, Endpoint};

    pub(in crate::cli) struct Listener {
        inner: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        /// 开始监听（要在核心的运行时中调用）。套接字还有服务在监听时（同一个数据目录的另一个服务）失败，
        /// 与 Windows 上管道名被占用时相同；没有服务在监听的是上次残留的文件，删掉重建。
        pub fn bind(endpoint: &Endpoint) -> io::Result<Self> {
            let path = endpoint.socket_path();
            if std::os::unix::net::UnixStream::connect(&path).is_ok() {
                return Err(io::ErrorKind::AddrInUse.into());
            }
            let _ = std::fs::remove_file(&path);
            let inner = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            Ok(Self { inner, path })
        }

        pub async fn accept(&mut self) -> io::Result<Box<dyn Conn>> {
            let (stream, _) = self.inner.accept().await?;
            Ok(Box::new(stream))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// 管道的安全设置（测试中核对）：属主是不是当前用户，以及 SDDL 写法的属主与 DACL 各项。SDDL 把众所周知的账户
/// 写成缩写（例如内置的 Administrator 是 `LA`，GitHub 的 Windows 运行器用的就是它），所以属主另按 SID 比较。
#[cfg(all(test, windows))]
pub(super) struct PipeSecurity {
    pub owner_is_current_user: bool,
    pub owner: String,
    pub aces: Vec<String>,
}

#[cfg(all(test, windows))]
pub(super) fn pipe_security(name: &str) -> std::io::Result<PipeSecurity> {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_SUCCESS, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
        GetSecurityInfo, SDDL_REVISION_1, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };

    let wide_to_string = |text: *mut u16| {
        // SAFETY: text 是 Win32 返回的以 0 结尾的字符串，读完后由调用方 LocalFree
        unsafe {
            let mut len = 0;
            while *text.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
        }
    };
    let me = imp::current_user_sid()?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(name)?;
    // SAFETY: GetSecurityInfo 写出的描述符用 LocalFree 释放（属主的 SID 指向其中，释放前用完）；转换出的字符串同样
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = ptr::null_mut();
        let mut owner_sid: PSID = ptr::null_mut();
        let err = GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner_sid,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        );
        if err != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let mut sid_text = ptr::null_mut();
        if ConvertSidToStringSidW(owner_sid, &mut sid_text) == 0 {
            let error = io::Error::last_os_error();
            LocalFree(sd);
            return Err(error);
        }
        let owner_is_current_user = wide_to_string(sid_text) == me;
        LocalFree(sid_text.cast());
        let mut text = ptr::null_mut();
        let ok = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            ptr::null_mut(),
        );
        LocalFree(sd);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let sddl = wide_to_string(text);
        LocalFree(text.cast());
        // O:<属主>D:P(A;;FA;;;<SID>)…
        let owner = sddl
            .strip_prefix("O:")
            .and_then(|rest| rest.split("D:").next())
            .unwrap_or_default()
            .to_string();
        let aces = sddl
            .split('(')
            .skip(1)
            .map(|ace| ace.trim_end_matches(')').to_string())
            .collect();
        Ok(PipeSecurity {
            owner_is_current_user,
            owner,
            aces,
        })
    }
}
