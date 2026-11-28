//! 资源管理器右键菜单中的“在此处打开 Termlet”（只在 Windows 上）：用默认的本地终端在这个文件夹打开一个 Tab。
//!
//! - 写在当前用户的 `HKCU\Software\Classes` 下（[`CLASSES`]），不需要管理员权限：`Directory\shell\Termlet` 是右键
//!   文件夹时的一项，`Directory\Background\shell\Termlet` 是在文件夹的空白处右键时的一项。每项的默认值是菜单上的
//!   文字，`Icon` 是程序本身，`command` 子项的默认值是命令行（[`command_line`]）。Windows 11 的新菜单中它们在
//!   “显示更多选项”里。
//! - 菜单指向的程序已不存在（便携版移动了位置、重装到了别处）时，Termlet 启动时改为指向自己（[`follow`]）；
//!   指向别处还在的 Termlet（另一份便携版）时不动，设置中可以改为指向这一个。
//! - 卸载程序删掉指向安装目录的这两项（`packaging/windows/helper.ps1` 的 `RemoveFolderMenu`）。
//!
//! 各函数的 `classes` 是 `HKCU` 下放这些键的位置，平时是 [`CLASSES`]；测试中换成临时的键，不碰真实的右键菜单。

use std::io;
use std::path::{Path, PathBuf};

/// 平时放这些键的位置（`HKCU` 下）。
pub const CLASSES: &str = r"Software\Classes";

/// 菜单项的键名（`shell` 下），发布后不能改：卸载程序按它删除。
pub const VERB: &str = "Termlet";

/// 两处菜单：右键文件夹、在文件夹的空白处右键。
#[cfg(windows)]
const PLACES: [&str; 2] = [r"Directory\shell", r"Directory\Background\shell"];

/// 菜单现在的样子。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuState {
    /// 没有（或者不完整：只有其中一处）
    Off,
    /// 两处都有，指向这个程序
    This,
    /// 指向别处的程序（另一份便携版、另一个安装位置；那里的程序可能已经不在）
    Other(PathBuf),
}

/// 这个平台是否支持。
pub fn supported() -> bool {
    cfg!(windows)
}

/// 菜单执行的命令行：`"<程序>" --open-folder "%V\."`。`%V` 是文件夹的路径；后面加 `\.`，因为盘符的根目录是
/// `C:\`，直接放进引号里会被当成转义的引号（拆出来是 `C:"`）。Termlet 解析参数时去掉多出来的 `.`。
pub fn command_line(exe: &Path) -> String {
    format!(r#""{}" --open-folder "%V\.""#, exe.display())
}

/// 命令行中的程序（第一项，可以用双引号括起来）。
fn program_of(command: &str) -> Option<PathBuf> {
    let command = command.trim_start();
    let program = match command.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"')?],
        None => command.split_whitespace().next()?,
    };
    (!program.is_empty()).then(|| PathBuf::from(program))
}

/// 两个程序路径是不是同一个（Windows 上不区分大小写）。
fn same_program(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// 按两处菜单中的命令行（没有的为 `None`）判断状态。
fn state_of(commands: [Option<String>; 2], exe: &Path) -> MenuState {
    let programs: Vec<Option<PathBuf>> = commands
        .iter()
        .map(|c| c.as_deref().and_then(program_of))
        .collect();
    if let Some(other) = programs.iter().flatten().find(|p| !same_program(p, exe)) {
        return MenuState::Other(other.clone());
    }
    if programs.iter().all(Option::is_some) {
        MenuState::This
    } else {
        MenuState::Off
    }
}

/// 现在的状态（`exe` 是这个程序）。
pub fn state(classes: &str, exe: &Path) -> io::Result<MenuState> {
    Ok(state_of(imp::commands(classes)?, exe))
}

/// 加上菜单（已有时改为指向这个程序），`label` 是菜单上的文字（按界面语言）。
pub fn enable(classes: &str, exe: &Path, label: &str) -> io::Result<()> {
    imp::write(classes, exe, Some(label))
}

/// 去掉菜单（没有时什么也不做）。
pub fn disable(classes: &str) -> io::Result<()> {
    imp::remove(classes)
}

/// 启动时调用：菜单指向的程序已不存在时改为指向 `exe`（菜单上的文字不变）。返回原来指向的程序，没有改动时为
/// `None`。没有菜单、指向这个程序或别处还在的程序时不动。
pub fn follow(classes: &str, exe: &Path) -> io::Result<Option<PathBuf>> {
    let MenuState::Other(old) = state(classes, exe)? else {
        return Ok(None);
    };
    if old.exists() {
        return Ok(None);
    }
    imp::write(classes, exe, None)?;
    Ok(Some(old))
}

#[cfg(windows)]
mod imp {
    use std::io;
    use std::path::Path;

    use windows_registry::CURRENT_USER;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND};

    use super::{PLACES, VERB, command_line};

    fn to_io(e: impl std::fmt::Display) -> io::Error {
        io::Error::other(e.to_string())
    }

    /// 键或值不存在的错误码（HRESULT）。
    fn not_found(code: i32) -> bool {
        [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND]
            .iter()
            .any(|c| code == (0x8007_0000 | c) as i32)
    }

    /// 两处菜单中的命令行，没有的为 `None`。
    pub(super) fn commands(classes: &str) -> io::Result<[Option<String>; 2]> {
        let read = |place: &str| -> io::Result<Option<String>> {
            let path = format!(r"{classes}\{place}\{VERB}\command");
            let key = match CURRENT_USER.options().read().open(&path) {
                Ok(key) => key,
                Err(e) if not_found(e.code().0) => return Ok(None),
                Err(e) => return Err(to_io(e)),
            };
            match key.get_string("") {
                Ok(command) => Ok(Some(command)),
                Err(e) if not_found(e.code().0) => Ok(None),
                Err(e) => Err(to_io(e)),
            }
        };
        Ok([read(PLACES[0])?, read(PLACES[1])?])
    }

    /// 写两处菜单：程序、图标与命令行；`label` 为 `None` 时菜单上的文字不变（没有时用程序名）。
    pub(super) fn write(classes: &str, exe: &Path, label: Option<&str>) -> io::Result<()> {
        let program = exe.display().to_string();
        let command = command_line(exe);
        for place in PLACES {
            let key = CURRENT_USER
                .create(format!(r"{classes}\{place}\{VERB}"))
                .map_err(to_io)?;
            match label {
                Some(label) => key.set_string("", label).map_err(to_io)?,
                None if key.get_string("").is_err() => key.set_string("", VERB).map_err(to_io)?,
                None => {}
            }
            key.set_string("Icon", &program).map_err(to_io)?;
            key.create("command")
                .and_then(|command_key| command_key.set_string("", &command))
                .map_err(to_io)?;
        }
        Ok(())
    }

    pub(super) fn remove(classes: &str) -> io::Result<()> {
        for place in PLACES {
            match CURRENT_USER.remove_tree(format!(r"{classes}\{place}\{VERB}")) {
                Err(e) if !not_found(e.code().0) => return Err(to_io(e)),
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;
    use std::path::Path;

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "只有 Windows 的资源管理器有这个菜单",
        )
    }

    pub(super) fn commands(_: &str) -> io::Result<[Option<String>; 2]> {
        Ok([None, None])
    }

    pub(super) fn write(_: &str, _: &Path, _: Option<&str>) -> io::Result<()> {
        Err(unsupported())
    }

    pub(super) fn remove(_: &str) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
