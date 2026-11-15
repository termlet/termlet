//! 把命令行工具所在的目录加入（或移出）当前用户的 PATH。
//!
//! - 只在 Windows 上自动修改（`HKCU\Environment\Path`，不需要管理员权限，保留原来的 REG_SZ / REG_EXPAND_SZ 类型，
//!   改完通知资源管理器等进程，之后新打开的终端即可使用 `termlet`）；其他平台只检测，修改时返回 `Unsupported`，
//!   由界面给出手动操作的说明。
//! - 安装目录根部是 `Termlet.exe`，命令行工具在 `bin\termlet.exe`（Windows 上文件名不分大小写，二者不能在同一目录），
//!   加入 PATH 的只有 `bin`（[`cli_dir`]）。
//! - 加入的目录另记在数据目录的 [`RECORD_FILE`] 中（[`record`]）：重装时换了安装位置，旧目录删除后，
//!   应用启动时由 [`follow`] 把 PATH 中的旧目录换成现在的目录（位置不变）。

use std::io;
use std::path::{Path, PathBuf};

/// 数据目录中记录"由 Termlet 加入 PATH 的目录"的文件。
pub const RECORD_FILE: &str = "cli-path.txt";

/// 命令行工具的文件名。
pub const CLI_EXE: &str = if cfg!(windows) {
    "termlet.exe"
} else {
    "termlet"
};

/// 命令行工具所在的目录：应用程序旁边的 `bin`。
pub fn cli_dir(app_exe: &Path) -> PathBuf {
    app_exe
        .parent()
        .map(|dir| dir.join("bin"))
        .unwrap_or_else(|| PathBuf::from("bin"))
}

/// 命令行工具的完整路径。
pub fn cli_path(app_exe: &Path) -> PathBuf {
    cli_dir(app_exe).join(CLI_EXE)
}

/// 这个平台能否由 Termlet 修改 PATH。
pub fn supported() -> bool {
    cfg!(windows)
}

/// 记下由 Termlet 加入 PATH 的目录；`None` 表示已移出（删除记录文件）。
pub fn record(file: &Path, dir: Option<&Path>) -> io::Result<()> {
    match dir {
        Some(dir) => crate::platform::write_private_file(file, dir.to_string_lossy().as_bytes()),
        None => match std::fs::remove_file(file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
    }
}

fn recorded(file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(file).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// `dir` 是否是安装版的命令行工具目录（`<安装目录>\bin`，旁边有 Termlet 程序）：开发构建不改动 PATH。
fn installed(dir: &Path) -> bool {
    let app = if cfg!(windows) {
        "Termlet.exe"
    } else {
        "Termlet"
    };
    dir.file_name()
        .is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("bin"))
        && dir.parent().is_some_and(|p| p.join(app).is_file())
}

/// 当前用户的 PATH 是否已包含 `dir`。
pub fn contains(dir: &Path) -> io::Result<bool> {
    let (list, _) = imp::read()?;
    Ok(list_contains(&list, dir))
}

/// 把 `dir` 加入当前用户的 PATH（已有时不变）。
pub fn add(dir: &Path) -> io::Result<()> {
    let (list, kind) = imp::read()?;
    let next = list_add(&list, dir);
    if next != list {
        imp::write(&next, kind)?;
    }
    Ok(())
}

/// 把 `dir` 从当前用户的 PATH 中移除（所有出现的位置）。
pub fn remove(dir: &Path) -> io::Result<()> {
    let (list, kind) = imp::read()?;
    let next = list_remove(&list, dir);
    if next != list {
        imp::write(&next, kind)?;
    }
    Ok(())
}

/// 应用启动时调用：记录中的目录已经没有命令行工具（重装到了别处、旧目录已删除）而仍在 PATH 中时，换成现在的
/// 目录 `dir`（位置不变）并更新记录。返回被替换的旧目录，没有改动时为 `None`。只对安装版生效；
/// 用户自己从 PATH 中删掉了时不再跟随（删除记录）。
pub fn follow(record_file: &Path, dir: &Path) -> io::Result<Option<PathBuf>> {
    let Some(old) = recorded(record_file) else {
        return Ok(None);
    };
    if !installed(dir) || !supported() {
        return Ok(None);
    }
    let (list, kind) = imp::read()?;
    if !list_contains(&list, &old) {
        record(record_file, None)?;
        return Ok(None);
    }
    let Some(next) = followed(&list, &old, dir) else {
        return Ok(None);
    };
    imp::write(&next, kind)?;
    record(record_file, Some(dir))?;
    Ok(Some(old))
}

// ---------------------------------------------------------------- PATH 列表

const SEP: char = if cfg!(windows) { ';' } else { ':' };

/// 比较两个目录：Windows 上不区分大小写，忽略末尾的分隔符。
fn same_dir(a: &str, b: &str) -> bool {
    let trim = |s: &str| s.trim_end_matches(['\\', '/']).to_string();
    let (a, b) = (trim(a), trim(b));
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b) || a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

fn entries(list: &str) -> impl Iterator<Item = &str> {
    list.split(SEP).filter(|p| !p.is_empty())
}

fn list_contains(list: &str, dir: &Path) -> bool {
    let dir = dir.to_string_lossy();
    entries(list).any(|p| same_dir(p, &dir))
}

/// 把 `dir` 追加到列表末尾（已有时原样返回）。
fn list_add(list: &str, dir: &Path) -> String {
    if list_contains(list, dir) {
        return list.to_string();
    }
    let trimmed = list.trim_end_matches(SEP);
    if trimmed.is_empty() {
        dir.to_string_lossy().into_owned()
    } else {
        format!("{trimmed}{SEP}{}", dir.to_string_lossy())
    }
}

/// 删除 `dir` 的所有出现，保留其余项的顺序。
fn list_remove(list: &str, dir: &Path) -> String {
    let dir = dir.to_string_lossy();
    entries(list)
        .filter(|p| !same_dir(p, &dir))
        .collect::<Vec<_>>()
        .join(&SEP.to_string())
}

/// 把列表中的 `old` 换成 `dir`，位置不变（重复的只留一个）；`dir` 已在列表中时只删除 `old`。
fn list_replace(list: &str, old: &Path, dir: &Path) -> String {
    if list_contains(list, dir) {
        return list_remove(list, old);
    }
    let old = old.to_string_lossy();
    let new = dir.to_string_lossy();
    let mut out: Vec<&str> = Vec::new();
    let mut done = false;
    for p in entries(list) {
        if same_dir(p, &old) {
            if !done {
                out.push(&new);
                done = true;
            }
            continue;
        }
        out.push(p);
    }
    out.join(&SEP.to_string())
}

/// [`follow`] 之后的 PATH：记录的目录 `old` 与现在的目录不同、`old` 中已经没有命令行工具、并且 `old` 仍在 PATH 中时，
/// 返回替换后的列表。`old` 中还有命令行工具（旧的安装仍在）时不动。
fn followed(list: &str, old: &Path, dir: &Path) -> Option<String> {
    if same_dir(&old.to_string_lossy(), &dir.to_string_lossy()) || !list_contains(list, old) {
        return None;
    }
    if old.join(CLI_EXE).exists() {
        return None;
    }
    Some(list_replace(list, old, dir))
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_registry::{CURRENT_USER, Type};
    use windows_sys::Win32::Foundation::ERROR_FILE_NOT_FOUND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    /// 值的类型：REG_EXPAND_SZ（默认）或 REG_SZ。
    #[derive(Clone, Copy)]
    pub(super) enum Kind {
        Expand,
        Plain,
    }

    fn to_io(e: impl std::fmt::Display) -> io::Error {
        io::Error::other(e.to_string())
    }

    pub(super) fn read() -> io::Result<(String, Kind)> {
        let key = CURRENT_USER
            .options()
            .read()
            .open("Environment")
            .map_err(to_io)?;
        let value = match key.get_value("Path") {
            Ok(value) => value,
            // 没有用户 PATH。其他错误（例如两次读取之间值变长了）照实报告：当作空的写回会丢掉用户原来的 PATH
            Err(e) if e.code().0 == (0x8007_0000 | ERROR_FILE_NOT_FOUND) as i32 => {
                return Ok((String::new(), Kind::Expand));
            }
            Err(e) => return Err(to_io(e)),
        };
        let kind = match value.ty() {
            Type::String => Kind::Plain,
            Type::ExpandString => Kind::Expand,
            other => {
                return Err(io::Error::other(format!(
                    "用户 PATH 的类型不是字符串（{other:?}），不修改"
                )));
            }
        };
        let wide = value.as_wide();
        let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
        Ok((String::from_utf16_lossy(&wide[..end]), kind))
    }

    pub(super) fn write(list: &str, kind: Kind) -> io::Result<()> {
        let key = CURRENT_USER
            .options()
            .read()
            .write()
            .open("Environment")
            .map_err(to_io)?;
        match kind {
            Kind::Plain => key.set_string("Path", list),
            Kind::Expand => key.set_expand_string("Path", list),
        }
        .map_err(to_io)?;
        broadcast();
        Ok(())
    }

    /// 通知资源管理器等进程环境变量已变化，之后新打开的终端即可使用 termlet。
    fn broadcast() {
        let name: Vec<u16> = "Environment".encode_utf16().chain(Some(0)).collect();
        let mut result = 0usize;
        // SAFETY: name 以 0 结尾，调用期间有效
        unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                0,
                name.as_ptr() as isize,
                SMTO_ABORTIFHUNG,
                5000,
                &mut result,
            );
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    #[derive(Clone, Copy)]
    pub(super) enum Kind {}

    /// 其他平台只检测当前进程的 PATH。
    pub(super) fn read() -> io::Result<(String, Kind)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "当前平台不支持自动修改 PATH",
        ))
    }

    pub(super) fn write(_: &str, kind: Kind) -> io::Result<()> {
        match kind {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[cfg(windows)]
    #[test]
    fn edits_path_lists() {
        let dir = p(r"C:\Users\u\AppData\Local\Programs\Termlet\bin");
        let list = r"C:\Windows;%USERPROFILE%\bin;";
        assert!(!list_contains(list, &dir));
        let added = list_add(list, &dir);
        assert_eq!(
            added,
            format!(r"C:\Windows;%USERPROFILE%\bin;{}", dir.display())
        );
        // 重复添加、大小写与结尾分隔符不同都视为同一目录
        assert_eq!(
            list_add(
                &added,
                &p(r"c:\users\U\AppData\Local\Programs\termlet\bin\")
            ),
            added
        );
        assert_eq!(
            list_remove(&format!("{added};{}", dir.display()), &dir),
            r"C:\Windows;%USERPROFILE%\bin"
        );
        assert_eq!(list_add("", &dir), dir.display().to_string());
    }

    #[cfg(windows)]
    #[test]
    fn replaces_in_place() {
        let old = r"C:\Users\u\AppData\Local\Programs\Termlet\bin";
        let dir = r"D:\Apps\Termlet\bin";
        assert_eq!(
            list_replace(&format!(r"C:\Windows;{old};C:\bin;{old}"), &p(old), &p(dir)),
            format!(r"C:\Windows;{dir};C:\bin"),
            "位置不变，重复的旧目录只留一个新目录"
        );
        assert_eq!(
            list_replace(
                &format!("{old};{dir}"),
                &p(r"c:\users\u\appdata\local\programs\termlet\bin\"),
                &p(dir)
            ),
            dir,
            "新目录已在列表中：只删除旧目录"
        );
    }

    #[test]
    fn follows_moved_installation() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("old").join("bin");
        let dir = root.path().join("new").join("bin");
        let list = format!(
            "{}{SEP}{}",
            if cfg!(windows) {
                r"C:\Windows"
            } else {
                "/usr/bin"
            },
            old.display()
        );
        // 旧目录中还有命令行工具（旧的安装仍在）：不动
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join(CLI_EXE), "").unwrap();
        assert_eq!(followed(&list, &old, &dir), None);
        // 旧目录已删除：换成现在的目录
        std::fs::remove_dir_all(root.path().join("old")).unwrap();
        let first = list.split(SEP).next().unwrap();
        assert_eq!(
            followed(&list, &old, &dir),
            Some(format!("{first}{SEP}{}", dir.display()))
        );
        // 与现在的目录相同、旧目录不在 PATH 中：不动
        assert_eq!(followed(&list, &dir, &dir), None);
        assert_eq!(followed(first, &old, &dir), None);
    }

    #[test]
    fn record_and_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(RECORD_FILE);
        assert_eq!(recorded(&file), None);
        record(&file, Some(&p(r"C:\a\bin"))).unwrap();
        assert_eq!(recorded(&file), Some(p(r"C:\a\bin")));
        record(&file, None).unwrap();
        assert_eq!(recorded(&file), None);
        record(&file, None).unwrap();

        let app = tmp.path().join("Termlet");
        std::fs::create_dir_all(app.join("bin")).unwrap();
        assert!(!installed(&app.join("bin")), "旁边没有 Termlet 程序");
        let exe = if cfg!(windows) {
            "Termlet.exe"
        } else {
            "Termlet"
        };
        std::fs::write(app.join(exe), "").unwrap();
        assert!(installed(&app.join("bin")));
        assert!(!installed(&app), "不是 bin 目录");
        assert_eq!(cli_dir(&app.join(exe)), app.join("bin"));
        assert_eq!(cli_path(&app.join(exe)), app.join("bin").join(CLI_EXE));
    }

    // 读取当前用户的 PATH（不修改）
    #[cfg(windows)]
    #[test]
    fn reads_user_path() {
        let (list, _) = imp::read().unwrap();
        assert!(!list_contains(&list, &p(r"Z:\definitely\not\in\path")));
    }
}
