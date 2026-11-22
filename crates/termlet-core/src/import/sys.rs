//! 与操作系统有关的部分：系统 ANSI 代码页，以及只有 Windows 才有的注册表
//! （Xshell 自定义的用户数据目录、FinalShell 的安装目录、安装版 MobaXterm 的书签）。

#[cfg(windows)]
mod imp {
    use std::path::PathBuf;

    use windows_registry::{CURRENT_USER, LOCAL_MACHINE};

    use super::super::ImportError;
    use super::super::mobaxterm::{MOBAXTERM_REGISTRY_SUBKEY, sort_bookmark_sections};

    /// 系统的 ANSI 代码页（与 `GetACP` 相同的来源）。
    pub fn ansi_code_page() -> Option<u32> {
        LOCAL_MACHINE
            .open(r"SYSTEM\CurrentControlSet\Control\Nls\CodePage")
            .ok()?
            .get_string("ACP")
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Xshell 选项中设置的用户数据目录（`HKCU\Software\NetSarang\Common\<版本>\UserData` 的 `UserDataPath`）。
    pub fn xshell_registry_data_path(version: &str) -> Option<PathBuf> {
        let key = CURRENT_USER
            .open(format!(r"Software\NetSarang\Common\{version}\UserData"))
            .ok()?;
        let path = expand_env(key.get_string("UserDataPath").ok()?.trim());
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    /// FinalShell 的安装目录（`HKCU\Software\finalshell` 的默认值）：FinalShell 把连接等数据保存在安装目录中。
    pub fn finalshell_registry_dir() -> Option<PathBuf> {
        let key = CURRENT_USER.open(r"Software\finalshell").ok()?;
        let path = expand_env(key.get_string("").ok()?.trim());
        (!path.is_empty()).then(|| PathBuf::from(path))
    }

    /// 读取安装版 MobaXterm 保存在注册表中的书签，转为与 MobaXterm.ini 相同的格式。
    /// 只读取 `Bookmarks`、`Bookmarks_N` 子键，不读取其他子键（例如 MobaXterm 保存密码的子键）。
    pub fn read_mobaxterm_registry() -> Result<String, ImportError> {
        let root = CURRENT_USER
            .open(MOBAXTERM_REGISTRY_SUBKEY)
            .map_err(|e| ImportError::Registry(e.message()))?;
        let names: Vec<String> = root
            .keys()
            .map_err(|e| ImportError::Registry(e.message()))?
            .collect();
        let mut text = String::new();
        for name in sort_bookmark_sections(names) {
            let Ok(section) = root.open(&name) else {
                continue;
            };
            text.push_str(&format!("[{name}]\n"));
            let Ok(values) = section.values() else {
                continue;
            };
            for (value_name, value) in values {
                if let Ok(s) = String::try_from(value) {
                    text.push_str(&format!("{value_name}={s}\n"));
                }
            }
        }
        Ok(text)
    }

    /// 注册表中是否有 MobaXterm 的书签。
    pub fn mobaxterm_registry_has_bookmarks() -> bool {
        CURRENT_USER
            .open(MOBAXTERM_REGISTRY_SUBKEY)
            .and_then(|root| root.keys().map(|keys| keys.collect::<Vec<_>>()))
            .is_ok_and(|names| !sort_bookmark_sections(names).is_empty())
    }

    /// 展开 `%VAR%` 形式的环境变量（REG_EXPAND_SZ 的值）；不存在的变量原样保留。
    fn expand_env(s: &str) -> String {
        let mut out = String::new();
        let mut rest = s;
        while let Some(start) = rest.find('%') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            match after.find('%') {
                Some(end) => {
                    let name = &after[..end];
                    match std::env::var(name) {
                        Ok(value) if !name.is_empty() => out.push_str(&value),
                        _ => out.push_str(&rest[start..start + end + 2]),
                    }
                    rest = &after[end + 1..];
                }
                None => {
                    out.push_str(&rest[start..]);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn expand_env_replaces_known_variables() {
            let windir = std::env::var("SystemRoot").unwrap();
            assert_eq!(expand_env(r"%SystemRoot%\x"), format!(r"{windir}\x"));
            assert_eq!(expand_env("%NO_SUCH_VAR_42%/a"), "%NO_SUCH_VAR_42%/a");
            assert_eq!(expand_env("100%"), "100%");
        }

        #[test]
        fn ansi_code_page_is_readable() {
            assert!(ansi_code_page().is_some_and(|cp| cp > 0));
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::PathBuf;

    use super::super::ImportError;

    /// 非 Windows 系统上无从得知配置文件来自哪个代码页，按 GB18030（GBK 的超集）处理。
    pub fn ansi_code_page() -> Option<u32> {
        None
    }

    pub fn xshell_registry_data_path(_version: &str) -> Option<PathBuf> {
        None
    }

    pub fn finalshell_registry_dir() -> Option<PathBuf> {
        None
    }

    pub fn read_mobaxterm_registry() -> Result<String, ImportError> {
        Err(ImportError::Registry(
            "只有 Windows 上的 MobaXterm 会把书签保存在注册表中".into(),
        ))
    }

    pub fn mobaxterm_registry_has_bookmarks() -> bool {
        false
    }
}

pub(super) use imp::*;
