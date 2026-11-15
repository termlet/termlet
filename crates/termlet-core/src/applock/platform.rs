//! 应用锁用到的系统功能：系统空闲时间、会话是否锁屏、系统验证（Windows Hello）。
//!
//! 都直接调用系统接口（不经 PowerShell：每次验证都启动一个 PowerShell 要约 1 秒）：
//!
//! - 空闲时间：`GetLastInputInfo`；
//! - 锁屏：GPUI 没有转发会话变化的消息，改为定时查询 `WTSQuerySessionInformationW(WTSSessionInfoEx)` 的锁定标志；
//! - Windows Hello：给了窗口时用 `IUserConsentVerifierInterop::RequestVerificationForWindowAsync`，验证框属于这个
//!   窗口、出现在它前面（不属于任何窗口的验证框有时出现在 Termlet 后面）；接口不可用（较早的 Windows 10）时
//!   退回 `UserConsentVerifier::RequestVerificationAsync`。
//!
//! 其他系统暂不支持（返回"不支持"，自动锁定与系统验证不可用）。测试与 UI 测试用 [`LockPlatform`] 换成假的。

use std::num::NonZeroIsize;
use std::time::Duration;

/// 系统验证的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    Verified,
    /// 用户取消了
    Canceled,
    /// 没有设备、没有为当前用户设置，或被策略禁用
    Unavailable,
    /// 设备正忙
    Busy,
    /// 失败次数太多，需要稍后再试
    RetriesExhausted,
    /// 其他错误（调用失败、超时）
    Failed,
}

/// 应用锁用到的系统功能。方法都是阻塞的，由应用锁在后台线程上调用。
pub trait LockPlatform: Send + Sync + 'static {
    /// 系统多久没有键盘、鼠标输入了；不支持时为 `None`。
    fn idle_time(&self) -> Option<Duration>;
    /// 当前会话是否处于锁屏状态；不支持或查询失败时为 `None`。
    fn session_locked(&self) -> Option<bool>;
    /// 能否用系统验证（Windows Hello）解锁。
    fn system_auth_available(&self) -> bool;
    /// 弹出系统验证，等用户完成。`window` 是验证框所属的窗口（Windows 上为 HWND）。
    fn system_verify(&self, window: Option<NonZeroIsize>, message: &str) -> Verification;
}

/// 当前系统的实现。
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemPlatform;

impl LockPlatform for SystemPlatform {
    fn idle_time(&self) -> Option<Duration> {
        imp::idle_time()
    }

    fn session_locked(&self) -> Option<bool> {
        imp::session_locked()
    }

    fn system_auth_available(&self) -> bool {
        imp::system_auth_available()
    }

    fn system_verify(&self, window: Option<NonZeroIsize>, message: &str) -> Verification {
        imp::system_verify(window, message)
    }
}

#[cfg(windows)]
mod imp {
    use std::num::NonZeroIsize;
    use std::time::Duration;

    use windows::Security::Credentials::UI::{
        UserConsentVerificationResult, UserConsentVerifier, UserConsentVerifierAvailability,
    };
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
        WTS_SESSIONSTATE_UNLOCK, WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW,
        WTSSessionInfoEx,
    };
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::System::WinRT::IUserConsentVerifierInterop;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
    use windows::core::{HSTRING, PWSTR};

    use super::Verification;

    pub fn idle_time() -> Option<Duration> {
        let mut info = LASTINPUTINFO {
            cbSize: size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        // SAFETY：info 的 cbSize 已按结构大小填好
        if !unsafe { GetLastInputInfo(&mut info) }.as_bool() {
            return None;
        }
        // 两者都是开机以来的毫秒数（32 位，约 49.7 天回绕一次），按回绕相减
        let now = unsafe { GetTickCount() };
        Some(Duration::from_millis(u64::from(
            now.wrapping_sub(info.dwTime),
        )))
    }

    pub fn session_locked() -> Option<bool> {
        let mut buffer = PWSTR::null();
        let mut bytes = 0u32;
        // SAFETY：成功时 buffer 指向系统分配的 WTSINFOEXW，用完以 WTSFreeMemory 释放
        unsafe {
            WTSQuerySessionInformationW(
                Some(WTS_CURRENT_SERVER_HANDLE),
                WTS_CURRENT_SESSION,
                WTSSessionInfoEx,
                &mut buffer,
                &mut bytes,
            )
        }
        .ok()?;
        if buffer.is_null() {
            return None;
        }
        let mut flags = None;
        if bytes as usize >= size_of::<WTSINFOEXW>() {
            // SAFETY：大小已检查；Level 为 1 时 Data 是 WTSINFOEX_LEVEL1_W
            let info = unsafe { &*(buffer.0 as *const WTSINFOEXW) };
            if info.Level == 1 {
                flags = Some(unsafe { info.Data.WTSInfoExLevel1.SessionFlags });
            }
        }
        unsafe { WTSFreeMemory(buffer.0.cast()) };
        // 标志在 Windows 7 / Server 2008 R2 上是反的，Termlet 只支持 Windows 10 起
        match flags? as u32 {
            WTS_SESSIONSTATE_LOCK => Some(true),
            WTS_SESSIONSTATE_UNLOCK => Some(false),
            _ => None,
        }
    }

    pub fn system_auth_available() -> bool {
        let result = UserConsentVerifier::CheckAvailabilityAsync().and_then(|op| op.join());
        match result {
            Ok(availability) => {
                tracing::info!(
                    availability = availability.0,
                    "Windows Hello 可用性（0 为可用）"
                );
                availability == UserConsentVerifierAvailability::Available
            }
            Err(e) => {
                tracing::info!("检查 Windows Hello 是否可用失败：{e}");
                false
            }
        }
    }

    pub fn system_verify(window: Option<NonZeroIsize>, message: &str) -> Verification {
        let message = HSTRING::from(message);
        let fallback = || UserConsentVerifier::RequestVerificationAsync(&message);
        let operation = match window {
            Some(hwnd) => {
                let for_window =
                    windows::core::factory::<UserConsentVerifier, IUserConsentVerifierInterop>()
                        .and_then(|interop| {
                            // SAFETY：hwnd 是调用方给出的、仍然存在的窗口
                            unsafe {
                                interop.RequestVerificationForWindowAsync(
                                    HWND(hwnd.get() as *mut core::ffi::c_void),
                                    &message,
                                )
                            }
                        });
                match for_window {
                    Ok(op) => Ok(op),
                    Err(e) => {
                        tracing::info!(
                            "无法让 Windows Hello 的验证框属于窗口，改用不属于窗口的：{e}"
                        );
                        fallback()
                    }
                }
            }
            None => fallback(),
        };
        match operation.and_then(|op| op.join()) {
            Ok(result) => match result {
                UserConsentVerificationResult::Verified => Verification::Verified,
                UserConsentVerificationResult::Canceled => Verification::Canceled,
                UserConsentVerificationResult::DeviceBusy => Verification::Busy,
                UserConsentVerificationResult::RetriesExhausted => Verification::RetriesExhausted,
                UserConsentVerificationResult::DeviceNotPresent
                | UserConsentVerificationResult::NotConfiguredForUser
                | UserConsentVerificationResult::DisabledByPolicy => Verification::Unavailable,
                _ => Verification::Failed,
            },
            Err(e) => {
                tracing::warn!("Windows Hello 验证失败：{e}");
                Verification::Failed
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::num::NonZeroIsize;
    use std::time::Duration;

    use super::Verification;

    pub fn idle_time() -> Option<Duration> {
        None
    }

    pub fn session_locked() -> Option<bool> {
        None
    }

    pub fn system_auth_available() -> bool {
        false
    }

    pub fn system_verify(_window: Option<NonZeroIsize>, _message: &str) -> Verification {
        Verification::Unavailable
    }
}
