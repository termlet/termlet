use super::*;

#[test]
fn command_line_quotes_the_program_and_the_folder() {
    let exe = Path::new(r"C:\Program Files\Termlet\Termlet.exe");
    let command = command_line(exe);
    assert_eq!(
        command,
        r#""C:\Program Files\Termlet\Termlet.exe" --open-folder "%V\.""#
    );
    assert_eq!(program_of(&command).as_deref(), Some(exe));
    assert_eq!(
        program_of(r"C:\Termlet\Termlet.exe --open-folder x").as_deref(),
        Some(Path::new(r"C:\Termlet\Termlet.exe"))
    );
    assert_eq!(program_of(r#""unterminated"#), None);
    assert_eq!(program_of("  "), None);
}

#[test]
fn state_follows_both_places() {
    let exe = Path::new(r"C:\Apps\Termlet\Termlet.exe");
    let this = Some(command_line(exe));
    let other = Some(command_line(Path::new(r"D:\Portable\Termlet\Termlet.exe")));
    assert_eq!(state_of([None, None], exe), MenuState::Off);
    assert_eq!(state_of([this.clone(), this.clone()], exe), MenuState::This);
    // 大小写不同的同一个程序
    let upper = Some(command_line(Path::new(r"c:\apps\termlet\TERMLET.EXE")));
    assert_eq!(state_of([upper, this.clone()], exe), MenuState::This);
    // 只有一处：不完整，打开开关时补齐
    assert_eq!(state_of([this.clone(), None], exe), MenuState::Off);
    // 有一处指向别处的
    assert_eq!(
        state_of([this, other], exe),
        MenuState::Other(PathBuf::from(r"D:\Portable\Termlet\Termlet.exe"))
    );
}

#[cfg(windows)]
mod registry {
    use windows_registry::CURRENT_USER;

    use super::*;

    /// 当前用户下的临时键（`Software\Termlet-test-<随机>`），代替 `Software\Classes`，用完删掉。
    struct TempClasses {
        root: String,
        classes: String,
    }

    impl TempClasses {
        fn new() -> Self {
            let root = format!(r"Software\Termlet-test-{}", crate::id::random_token(10));
            let classes = format!(r"{root}\Classes");
            CURRENT_USER.create(&classes).expect("建临时键");
            Self { root, classes }
        }

        fn string(&self, path: &str) -> Option<String> {
            CURRENT_USER
                .open(format!(r"{}\{path}", self.classes))
                .ok()?
                .get_string("")
                .ok()
        }

        fn icon(&self, place: &str) -> Option<String> {
            CURRENT_USER
                .open(format!(r"{}\{place}\{VERB}", self.classes))
                .ok()?
                .get_string("Icon")
                .ok()
        }
    }

    impl Drop for TempClasses {
        fn drop(&mut self) {
            let _ = CURRENT_USER.remove_tree(&self.root);
        }
    }

    // 打开：两处都写好文字、图标与命令行；再打开改为指向别的程序；关闭：两处都删掉，再关闭也不报错
    #[test]
    fn enables_and_disables_under_a_temp_key() {
        let temp = TempClasses::new();
        let classes = temp.classes.as_str();
        let exe = Path::new(r"C:\Apps\Termlet\Termlet.exe");
        assert_eq!(state(classes, exe).unwrap(), MenuState::Off);

        enable(classes, exe, "在此处打开 Termlet").unwrap();
        assert_eq!(state(classes, exe).unwrap(), MenuState::This);
        for place in PLACES {
            assert_eq!(
                temp.string(&format!(r"{place}\{VERB}")).as_deref(),
                Some("在此处打开 Termlet")
            );
            assert_eq!(
                temp.icon(place).as_deref(),
                Some(r"C:\Apps\Termlet\Termlet.exe")
            );
            assert_eq!(
                temp.string(&format!(r"{place}\{VERB}\command")),
                Some(command_line(exe))
            );
        }

        let moved = Path::new(r"D:\Termlet\Termlet.exe");
        assert_eq!(
            state(classes, moved).unwrap(),
            MenuState::Other(exe.to_path_buf())
        );
        enable(classes, moved, "Open Termlet here").unwrap();
        assert_eq!(state(classes, moved).unwrap(), MenuState::This);

        disable(classes).unwrap();
        assert_eq!(state(classes, moved).unwrap(), MenuState::Off);
        for place in PLACES {
            assert_eq!(temp.string(&format!(r"{place}\{VERB}")), None);
        }
        disable(classes).unwrap();
    }

    // 启动时跟随：指向的程序还在时不动；已不存在（便携版移动了位置）时改为指向自己，菜单上的文字不变
    #[test]
    fn follow_retargets_only_a_missing_program() {
        let temp = TempClasses::new();
        let classes = temp.classes.as_str();
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old").join("Termlet.exe");
        std::fs::create_dir_all(old.parent().unwrap()).unwrap();
        std::fs::write(&old, b"").unwrap();
        let exe = dir.path().join("new").join("Termlet.exe");

        assert_eq!(follow(classes, &exe).unwrap(), None, "没有菜单时不动");
        enable(classes, &old, "在此处打开 Termlet").unwrap();
        assert_eq!(follow(classes, &exe).unwrap(), None, "原来的程序还在");
        assert_eq!(state(classes, &exe).unwrap(), MenuState::Other(old.clone()));

        std::fs::remove_dir_all(old.parent().unwrap()).unwrap();
        assert_eq!(follow(classes, &exe).unwrap(), Some(old));
        assert_eq!(state(classes, &exe).unwrap(), MenuState::This);
        assert_eq!(
            temp.string(&format!(r"{}\{VERB}", PLACES[1])).as_deref(),
            Some("在此处打开 Termlet")
        );
        assert_eq!(follow(classes, &exe).unwrap(), None, "已经指向自己");
    }

    /// 带 BOM 的 helper.ps1（打包时 release.mjs 也加 BOM；没有 BOM 时 Windows PowerShell 按系统代码页读）。
    fn helper_script(dir: &Path) -> PathBuf {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/windows/helper.ps1");
        let text = std::fs::read(&source).expect("读 helper.ps1");
        let script = dir.join("helper.ps1");
        let mut data = b"\xEF\xBB\xBF".to_vec();
        data.extend(text);
        std::fs::write(&script, data).unwrap();
        script
    }

    // 卸载程序（helper.ps1 的 RemoveFolderMenu）：删掉指向安装目录的菜单，指向别处的（另一份便携版）不动
    #[test]
    fn uninstaller_removes_only_its_own_menu() {
        let temp = TempClasses::new();
        let classes = temp.classes.as_str();
        let dir = tempfile::tempdir().unwrap();
        let script = helper_script(dir.path());
        let install = dir.path().join("Termlet 安装目录");
        let run = |install: &Path| {
            let status = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&script)
                .args(["-Action", "RemoveFolderMenu", "-Dir"])
                .arg(install)
                .args(["-Classes", classes])
                .status()
                .expect("运行 helper.ps1");
            assert!(status.success());
        };

        let elsewhere = dir.path().join("便携版").join("Termlet.exe");
        enable(classes, &elsewhere, "在此处打开 Termlet").unwrap();
        run(&install);
        assert_eq!(
            state(classes, &elsewhere).unwrap(),
            MenuState::This,
            "指向别处的不动"
        );

        let ours = install.join("Termlet.exe");
        enable(classes, &ours, "在此处打开 Termlet").unwrap();
        run(&install);
        assert_eq!(state(classes, &ours).unwrap(), MenuState::Off);
        for place in PLACES {
            assert_eq!(temp.string(&format!(r"{place}\{VERB}")), None);
        }

        // 菜单中写的程序与安装目录一边是短文件名、一边是长文件名（CI 的临时目录是 C:\Users\RUNNER~1\…，Windows
        // PowerShell 的 GetFullPath 会把它展开）：也认得出是同一个
        std::fs::create_dir_all(&install).unwrap();
        let (short, long) = (short_path(&install), long_path(&install));
        if short == long {
            println!("临时目录所在的卷没有短文件名，只测了写法相同的情况");
        }
        for (written, given) in [(&short, &long), (&long, &short)] {
            let program = written.join("Termlet.exe");
            enable(classes, &program, "在此处打开 Termlet").unwrap();
            run(given);
            assert_eq!(
                state(classes, &program).unwrap(),
                MenuState::Off,
                "菜单中是 {}，安装目录是 {}",
                written.display(),
                given.display()
            );
        }
    }

    /// Windows 的路径换算（`GetShortPathNameW` / `GetLongPathNameW`），换不了时原样返回。
    fn convert_path(
        path: &Path,
        convert: unsafe extern "system" fn(*const u16, *mut u16, u32) -> u32,
    ) -> PathBuf {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: wide 以 0 结尾；buf 的长度如实传入，返回值不超过它时是写入的字符数
        let len = unsafe { convert(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
        if len == 0 || len >= buf.len() {
            return path.to_path_buf();
        }
        PathBuf::from(std::ffi::OsString::from_wide(&buf[..len]))
    }

    fn short_path(path: &Path) -> PathBuf {
        convert_path(
            path,
            windows_sys::Win32::Storage::FileSystem::GetShortPathNameW,
        )
    }

    fn long_path(path: &Path) -> PathBuf {
        convert_path(
            path,
            windows_sys::Win32::Storage::FileSystem::GetLongPathNameW,
        )
    }

    // 菜单中写的是同一个程序的短文件名：状态照样是“指向这个程序”（启动时不当成别处的、不跟随）
    #[test]
    fn short_and_long_names_are_the_same_program() {
        let temp = TempClasses::new();
        let classes = temp.classes.as_str();
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("Termlet 安装目录");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("Termlet.exe"), b"").unwrap();
        let short = short_path(&install).join("Termlet.exe");
        let long = long_path(&install).join("Termlet.exe");
        enable(classes, &short, "在此处打开 Termlet").unwrap();
        assert_eq!(state(classes, &long).unwrap(), MenuState::This);
        assert_eq!(follow(classes, &long).unwrap(), None);
        enable(classes, &long, "在此处打开 Termlet").unwrap();
        assert_eq!(state(classes, &short).unwrap(), MenuState::This);
    }
}

// 资源管理器把 %V 换成文件夹的路径后，按 Windows 的命令行规则拆出的参数：空格、中文、盘符根目录、网络路径都完整
// （多出的 `\.` 由 Termlet 解析参数时去掉）
#[test]
fn explorer_command_splits_into_the_folder() {
    use crate::localshell::cmdline::{split_command, split_windows_args};
    let exe = r"C:\Program Files\Termlet\Termlet.exe";
    for folder in [r"D:\my work", r"C:\", r"E:\中文 目录", r"\server\share\a b"] {
        let line = command_line(Path::new(exe)).replace("%V", folder);
        let (program, rest) = split_command(&line);
        assert_eq!(program, exe);
        assert_eq!(
            split_windows_args(rest),
            ["--open-folder".to_string(), format!(r"{folder}\.")],
            "{line}"
        );
    }
}
