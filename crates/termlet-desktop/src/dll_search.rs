//! 限定按名字加载 DLL 时的搜索位置（Windows）：启动时最先调用 [`restrict`]，之后本进程按名字加载的 DLL
//! 只在程序目录与 System32 中找，不再找当前目录与 PATH，别处放的同名 DLL 不会被加载（DLL 植入）。
//!
//! 按名字加载的有：portable-pty 的 conpty.dll（安装包与便携版附带在程序旁边；用户删掉它换回系统自带的 ConPTY 时，
//! 当前目录、PATH 中的同名文件也不会顶上），GPUI 读显卡驱动版本用的 nvapi64.dll（随驱动装在 System32）与
//! amd_ags_x64.dll（不是系统文件，以前只在当前目录或 PATH 中碰巧有时才加载到，现在读不到 AMD 驱动的版本号，
//! 它只用于诊断）。DirectWrite、输入法等都是 System32 中的系统 DLL；输入法、Shell 扩展等 COM 组件按完整路径加载，
//! 它们同目录的依赖照常找得到（COM 加载时用 LOAD_WITH_ALTERED_SEARCH_PATH，不受这里影响）。其他系统上什么也不做。

/// 把本进程按名字加载 DLL 的搜索位置限定为程序目录与 System32（`LOAD_LIBRARY_SEARCH_DEFAULT_DIRS`，另含
/// `AddDllDirectory` 加的目录，Termlet 没有加）。要在加载任何 DLL 之前调用：`main` 的第一句。
pub fn restrict() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::LibraryLoader::{
            LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, SetDefaultDllDirectories,
        };
        // 失败只可能是参数不对；这时照旧按系统默认的顺序搜索，不影响启动（日志还没开始，不记）
        // SAFETY: 只传入文档列出的标志，没有指针参数
        unsafe {
            SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use windows_sys::Win32::Foundation::{ERROR_BAD_EXE_FORMAT, ERROR_MOD_NOT_FOUND, GetLastError};
    use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, LoadLibraryW};

    use super::restrict;

    /// 子进程中运行 [`planted_dlls_child`] 的标记（值不用）
    const CHILD_ENV: &str = "TERMLET_DLL_SEARCH_CHILD";
    /// 只放在当前目录中的假 DLL
    const CWD_PROBE: &str = "termlet-planted-cwd.dll";
    /// 只放在 PATH 中一个目录里的假 DLL
    const PATH_PROBE: &str = "termlet-planted-path.dll";

    /// 按名字加载 DLL：成功时返回加载到的文件，失败时返回错误码。
    fn load(name: &str) -> Result<PathBuf, u32> {
        let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
        // SAFETY: wide 以 0 结尾；加载成功的模块不释放，随测试进程结束
        let module = unsafe { LoadLibraryW(wide.as_ptr()) };
        if module.is_null() {
            // SAFETY: 紧接在失败的调用之后读本线程的错误码
            return Err(unsafe { GetLastError() });
        }
        let mut buf = vec![0u16; 1024];
        // SAFETY: buf 的长度如实传入
        let len = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) };
        Ok(PathBuf::from(String::from_utf16_lossy(
            &buf[..len as usize],
        )))
    }

    /// 当前目录与 PATH 中各放一个假的 DLL（内容不是程序），另在当前目录放一个假的 conpty.dll，在子进程中
    /// （限定搜索位置、改当前目录与 PATH 都对整个进程起作用，不能影响并行的其他测试）核对：限定之前它们会被
    /// 找到，限定之后找不到，conpty.dll 只会是测试程序旁边的那个，系统 DLL 照常加载。
    #[test]
    fn planted_dlls_are_not_loaded() {
        let dir = std::env::temp_dir().join(format!("termlet-dll-search-{}", std::process::id()));
        let on_path = dir.join("on-path");
        std::fs::create_dir_all(&on_path).unwrap();
        for file in [
            dir.join(CWD_PROBE),
            dir.join("conpty.dll"),
            on_path.join(PATH_PROBE),
        ] {
            std::fs::write(file, b"not a dll").unwrap();
        }
        let path = std::env::join_paths(std::iter::once(on_path.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "dll_search::tests::planted_dlls_child",
                "--ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .current_dir(&dir)
            .env(CHILD_ENV, "1")
            .env("PATH", path)
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success() && output.contains("1 passed"),
            "子进程：{output}"
        );
    }

    #[test]
    #[ignore = "只在 planted_dlls_are_not_loaded 启动的子进程中运行"]
    fn planted_dlls_child() {
        if std::env::var_os(CHILD_ENV).is_none() {
            return;
        }
        // 限定之前：按系统默认的顺序会找到当前目录与 PATH 中的假 DLL（不是合法的程序）
        assert_eq!(load(CWD_PROBE), Err(ERROR_BAD_EXE_FORMAT));
        assert_eq!(load(PATH_PROBE), Err(ERROR_BAD_EXE_FORMAT));

        restrict();
        assert_eq!(load(CWD_PROBE), Err(ERROR_MOD_NOT_FOUND));
        assert_eq!(load(PATH_PROBE), Err(ERROR_MOD_NOT_FOUND));
        // 测试程序旁边有随带的 conpty.dll 时（CI）加载的是它，没有时找不到；当前目录中的假文件都不会被加载
        let exe = std::env::current_exe().unwrap();
        let beside = exe.parent().unwrap().join("conpty.dll");
        match load("conpty.dll") {
            Ok(path) => assert!(same_file(&path, &beside), "加载的是 {}", path.display()),
            Err(code) => assert_eq!(code, ERROR_MOD_NOT_FOUND),
        }
        // 系统 DLL 照常加载
        for name in ["dwrite.dll", "uxtheme.dll", "imm32.dll"] {
            let path = load(name).unwrap_or_else(|code| panic!("{name} 加载失败：{code}"));
            assert!(
                path.to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(r"\system32\"),
                "{name} 加载自 {}",
                path.display()
            );
        }
    }

    fn same_file(a: &Path, b: &Path) -> bool {
        a.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .eq_ignore_ascii_case(&b.to_string_lossy())
    }
}
