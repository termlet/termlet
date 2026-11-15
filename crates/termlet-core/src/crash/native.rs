//! Windows 上的原生崩溃：访问冲突、栈溢出、非法指令这类不经过 panic 的崩溃，保存小型内存转储并写一条崩溃记录。
//!
//! - [`install`] 设置顶层异常过滤器（`SetUnhandledExceptionFilter`），并预先建好一个等待的线程。崩溃时过滤器只记下
//!   异常、通知那个线程、等它写完（最多 [`WRITE_TIMEOUT_MS`]），然后交给之前的过滤器或系统（照常结束进程）。转储在
//!   另一个线程里写：栈溢出时出错的线程已经没有栈可用；过滤器自己不分配内存。
//! - 转储放在 `{日志目录}/dumps/`，文件名 `termlet-<UTC 时间>-<进程号>.dmp`，最多保留 [`MAX_DUMPS`] 份。类型是小型转储
//!   加线程信息与卸载的模块：只有各线程的栈和加载的模块，不含整个堆。只保存在本机，自动上报不发送它
//!   （栈上可能有内存中的片段，发给别人前由用户自己决定）。
//! - 只处理第一次崩溃；快速失败（`__fastfail`，0xC0000409，Rust 的 abort 也是它）不经过过滤器，由系统处理；
//!   Rust 的 panic 已经由 panic hook 记录。

use std::fs::{self, File};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpNormal, MiniDumpWithThreadInfo,
    MiniDumpWithUnloadedModules, MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId, INFINITE, SetEvent,
    WaitForSingleObject,
};

/// 最多保留的转储份数（小型转储每份几 MB）。
pub const MAX_DUMPS: usize = 5;
/// 等写转储的线程写完的时限。
pub const WRITE_TIMEOUT_MS: u32 = 30_000;
pub use super::DUMP_DIR;

/// 写完转储之后，用异常代码与转储的文件名写崩溃记录（在写转储的线程上调用）。
pub type OnCrash = fn(code: u32, dump: Option<&str>);

/// 过滤器与写转储的线程之间的约定：事件句柄存成整数（句柄本身不是 `Send`）。
struct State {
    request: usize,
    done: usize,
}

static STATE: OnceLock<State> = OnceLock::new();
static EXCEPTION: AtomicPtr<EXCEPTION_POINTERS> = AtomicPtr::new(null_mut());
static THREAD: AtomicU32 = AtomicU32::new(0);
static CRASHED: AtomicBool = AtomicBool::new(false);
/// 之前设置的过滤器（函数指针存成整数，0 为没有）
static PREVIOUS: AtomicUsize = AtomicUsize::new(0);

/// 设置过滤器，转储写到 `logs_dir/dumps/`。只有第一次调用生效。
pub fn install(logs_dir: &Path, on_crash: OnCrash) {
    let dir = logs_dir.join(DUMP_DIR);
    prune(&dir, MAX_DUMPS);
    // 自动复位的事件，初始无信号
    let request = unsafe { CreateEventW(null(), 0, 0, null()) };
    let done = unsafe { CreateEventW(null(), 0, 0, null()) };
    if request.is_null() || done.is_null() {
        tracing::warn!("无法设置原生崩溃的记录：创建事件失败");
        return;
    }
    let state = State {
        request: request as usize,
        done: done as usize,
    };
    if STATE.set(state).is_err() {
        return;
    }
    let (request, done) = (request as usize, done as usize);
    let spawned = std::thread::Builder::new()
        .name("termlet-crash".into())
        .spawn(move || writer(dir, request, done, on_crash));
    if let Err(e) = spawned {
        tracing::warn!("无法设置原生崩溃的记录：{e}");
        return;
    }
    let previous = unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    if let Some(previous) = previous {
        PREVIOUS.store(previous as usize, Ordering::SeqCst);
    }
}

/// 顶层异常过滤器：只记下异常、交给写转储的线程。
unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    if !CRASHED.swap(true, Ordering::SeqCst)
        && let Some(state) = STATE.get()
    {
        EXCEPTION.store(info as *mut EXCEPTION_POINTERS, Ordering::SeqCst);
        THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::SeqCst);
        unsafe {
            SetEvent(state.request as HANDLE);
            WaitForSingleObject(state.done as HANDLE, WRITE_TIMEOUT_MS);
        }
    }
    let previous = PREVIOUS.load(Ordering::SeqCst);
    if previous == 0 {
        // EXCEPTION_CONTINUE_SEARCH：交给系统，照常结束进程
        return 0;
    }
    // SAFETY：PREVIOUS 中存的是 SetUnhandledExceptionFilter 返回的过滤器函数指针
    let previous = unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32>(
            previous,
        )
    };
    unsafe { previous(info) }
}

/// 写转储的线程：一直等，崩溃时写转储与记录，然后放行过滤器。
fn writer(dir: PathBuf, request: usize, done: usize, on_crash: OnCrash) {
    unsafe { WaitForSingleObject(request as HANDLE, INFINITE) };
    let pointers = EXCEPTION.load(Ordering::SeqCst);
    let thread = THREAD.load(Ordering::SeqCst);
    let code = if pointers.is_null() {
        0
    } else {
        // SAFETY：过滤器传来的指针在它等待期间一直有效
        unsafe {
            let record = (*pointers).ExceptionRecord;
            if record.is_null() {
                0
            } else {
                (*record).ExceptionCode as u32
            }
        }
    };
    let dump = write_dump(&dir, pointers, thread);
    on_crash(code, dump.as_deref());
    unsafe { SetEvent(done as HANDLE) };
}

/// 写一份小型转储，返回文件名；写不了时返回 `None`（删掉写了一半的文件）。
fn write_dump(dir: &Path, pointers: *mut EXCEPTION_POINTERS, thread: u32) -> Option<String> {
    fs::create_dir_all(dir).ok()?;
    let name = format!(
        "termlet-{}-{}.dmp",
        super::file_stamp(crate::time::now_ms()),
        std::process::id()
    );
    let path = dir.join(&name);
    let file = File::create(&path).ok()?;
    let info = MINIDUMP_EXCEPTION_INFORMATION {
        ThreadId: thread,
        ExceptionPointers: pointers,
        ClientPointers: 0,
    };
    let ok = unsafe {
        MiniDumpWriteDump(
            GetCurrentProcess(),
            GetCurrentProcessId(),
            file.as_raw_handle() as HANDLE,
            MiniDumpNormal | MiniDumpWithThreadInfo | MiniDumpWithUnloadedModules,
            if pointers.is_null() { null() } else { &info },
            null(),
            null(),
        )
    };
    drop(file);
    if ok == 0 {
        let _ = fs::remove_file(&path);
        return None;
    }
    prune(dir, MAX_DUMPS);
    Some(name)
}

/// 只留最新的 `keep` 份转储（按修改时间）。
pub fn prune(dir: &Path, keep: usize) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut dumps: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "dmp"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    dumps.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
    for (_, path) in dumps.into_iter().skip(keep) {
        let _ = fs::remove_file(path);
    }
}

/// 异常代码的说明（崩溃记录中显示）。
pub fn describe(code: u32) -> String {
    let name = match code {
        0xC000_0005 => "访问冲突（ACCESS_VIOLATION）",
        0xC000_00FD => "栈溢出（STACK_OVERFLOW）",
        0xC000_001D => "非法指令（ILLEGAL_INSTRUCTION）",
        0xC000_0094 => "整数除以零（INT_DIVIDE_BY_ZERO）",
        0xC000_0096 => "特权指令（PRIV_INSTRUCTION）",
        0xC000_0006 => "页面错误（IN_PAGE_ERROR）",
        0xC000_0374 => "堆损坏（HEAP_CORRUPTION）",
        0xC000_0409 => "栈缓冲区溢出（STACK_BUFFER_OVERRUN）",
        0x8000_0003 => "断点（BREAKPOINT）",
        0xE06D_7363 => "C++ 异常",
        _ => "未知异常",
    };
    format!("0x{code:08X} {name}")
}
