//! 批量执行中的 Ctrl+C（与 pdsh 相同）：第一次列出还没有结束的主机，再按一次取消全部——关闭到 Termlet 的连接，
//! Termlet 中止进行中的命令（远程命令收到 SIGTERM）。只在批量执行期间接管，其余时候照系统默认直接结束。
//!
//! Windows 用 `SetConsoleCtrlHandler`（回调在单独的线程中执行）；Unix 在主线程屏蔽 SIGINT，另起一个线程用 `sigwait`
//! 接收（信号处理函数中不能加锁）。批量执行结束后再收到 Ctrl+C 时以 130 退出（与 shell 相同）。

use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use termlet_proto::client::Abort;

use crate::i18n::Lang;
use crate::messages;

struct Shared {
    presses: u32,
    /// 还没有结束的主机的名称（按选中的顺序）
    unfinished: Vec<String>,
    abort: Option<Abort>,
    lang: Lang,
}

/// 正在进行的批量执行（没有时按默认处理）。
static ACTIVE: Mutex<Option<Arc<Mutex<Shared>>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// 接管 Ctrl+C 期间的句柄；丢弃时恢复默认处理。
pub struct Watch {
    shared: Arc<Mutex<Shared>>,
}

impl Watch {
    /// 开始接管。`abort` 是到 Termlet 的连接的中止句柄（取不到时第二次 Ctrl+C 直接结束本进程）。
    pub fn start(abort: Option<Abort>, lang: Lang) -> Self {
        install_once();
        let shared = Arc::new(Mutex::new(Shared {
            presses: 0,
            unfinished: Vec::new(),
            abort,
            lang,
        }));
        *lock(&ACTIVE) = Some(shared.clone());
        Self { shared }
    }

    /// 更新还没有结束的主机。
    pub fn set_unfinished(&self, names: Vec<String>) {
        lock(&self.shared).unfinished = names;
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut active = lock(&ACTIVE);
        if active
            .as_ref()
            .is_some_and(|a| Arc::ptr_eq(a, &self.shared))
        {
            *active = None;
        }
    }
}

/// 收到一次 Ctrl+C：有进行中的批量执行时处理并返回真，否则返回假（照默认结束）。
fn on_interrupt() -> bool {
    let Some(shared) = lock(&ACTIVE).clone() else {
        return false;
    };
    let mut state = lock(&shared);
    state.presses += 1;
    let lang = state.lang;
    if state.presses == 1 {
        let names = &state.unfinished;
        eprintln!(
            "\n[termlet] {}",
            messages::BATCH_UNFINISHED.fill(lang, &[&names.len(), &names.join(", ")])
        );
        eprintln!("[termlet] {}", messages::BATCH_INTERRUPT_HINT.get(lang));
    } else if state.presses == 2 {
        eprintln!("\n[termlet] {}", messages::BATCH_CANCELING.get(lang));
        match state.abort.take() {
            Some(abort) => abort.abort(),
            None => std::process::exit(130),
        }
    }
    true
}

fn install_once() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(install);
}

#[cfg(windows)]
fn install() {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };

    unsafe extern "system" fn handler(kind: u32) -> windows_sys::core::BOOL {
        if (kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT) && on_interrupt() {
            1
        } else {
            0
        }
    }
    // SAFETY: 注册一个进程级的回调；回调只访问 Mutex 保护的状态
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(unix)]
fn install() {
    // SAFETY: 标准的信号集操作；只改本线程（主线程）的屏蔽字，之后起的线程继承它
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
            return;
        }
        let set_for_thread = set;
        std::thread::spawn(move || {
            loop {
                let mut signal: libc::c_int = 0;
                if libc::sigwait(&set_for_thread, &mut signal) != 0 {
                    continue;
                }
                if signal == libc::SIGINT && !on_interrupt() {
                    std::process::exit(130);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 没有进行中的批量执行时不接管；第一次列出、第二次取消（没有连接时直接结束，这里不测）
    #[test]
    fn presses_are_counted_while_active() {
        assert!(!on_interrupt());
        let watch = Watch::start(None, Lang::Zh);
        watch.set_unfinished(vec!["web-1".into(), "web-2".into()]);
        assert!(on_interrupt());
        assert_eq!(lock(&watch.shared).presses, 1);
        drop(watch);
        assert!(!on_interrupt(), "结束后恢复默认");
    }
}
