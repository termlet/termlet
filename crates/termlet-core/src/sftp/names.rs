//! 文件名：远端给的名称在本机能否直接使用（SFTP 下载与终端中的文件传输共用），
//! 传输中的临时文件名、重名时的备选名称。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 传输中的临时文件后缀（上传在远程、下载在本机），完成后改名为正式文件名。
/// 失败时保留，继续传输时从中断处接着传；取消时删除。
pub const PART_SUFFIX: &str = ".termlet-part";

/// 文件名的长度上限：Linux 等多数文件系统是 255 字节，NTFS 是 255 个 UTF-16 单位。
pub const MAX_NAME_LEN: usize = 255;

/// Windows 的保留设备名。
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 是否是 Windows 的保留设备名（不区分大小写，带扩展名也算，例如 `con.txt`、`NUL.tar.gz`）：
/// 在 Windows 上写这样的文件会写到设备上而不是磁盘上。其他平台上同样避开，保持一致。
pub fn is_reserved_name(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or(name).trim();
    RESERVED.iter().any(|r| r.eq_ignore_ascii_case(base))
}

/// 远端的名称在本机使用时的写法：替换 Windows 文件名中不允许的字符与控制字符，去掉末尾的点与空格，
/// 拒绝 `.`、`..` 这类会改变目录的名称（换成 `_`），保留设备名（CON、NUL 等）前加 `_`。
pub fn safe_name(name: &str) -> String {
    let mapped: String = name
        .chars()
        .map(|c| {
            if (c as u32) < 32 || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = mapped.trim_end_matches([' ', '.']);
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return "_".into();
    }
    if is_reserved_name(trimmed) {
        return format!("_{trimmed}");
    }
    trimmed.to_string()
}

/// 文件 `name` 的临时文件名：通常是"名称.termlet-part"，`n > 0` 时（同名的已被别的任务占用）为"名称.n.termlet-part"。
/// 加上后缀后超过长度上限时（原名已接近上限，例如 82 个汉字加 .pdf 共 250 字节）改用".<名称的哈希>.termlet-part"：
/// 同一个文件总是得到同一个临时文件名，继续传输、清理时都能按原名找到它。
pub(crate) fn part_name(name: &str, n: u32, fits: fn(&str) -> bool) -> String {
    let tag = if n > 0 {
        format!(".{n}")
    } else {
        String::new()
    };
    tagged_name(name, &tag, PART_SUFFIX, fits)
}

/// `name` 加上 `tag` 与后缀 `suffix` 的文件名；超过长度上限时用名称的哈希代替名称。
pub(crate) fn tagged_name(name: &str, tag: &str, suffix: &str, fits: fn(&str) -> bool) -> String {
    let full = format!("{name}{tag}{suffix}");
    if fits(&full) {
        return full;
    }
    let sum = Sha256::digest(name.as_bytes());
    format!(
        ".{}{tag}{suffix}",
        data_encoding::HEXLOWER.encode(&sum[..8])
    )
}

/// 上传 `remote` 时的第 `n` 个备选临时文件（与它在同一目录）。远程按字节计算文件名长度。
pub(crate) fn remote_part(remote: &str, n: u32) -> String {
    let (dir, name) = super::path::split(remote);
    format!("{dir}{}", part_name(name, n, remote_name_fits))
}

/// 上传 `remote` 时用的一次性临时文件"名称.<随机>.termlet-part"（命令行工具的单文件上传用）。
pub(crate) fn remote_temp_part(remote: &str) -> String {
    let (dir, name) = super::path::split(remote);
    let tag = format!(".{}", random_hex(4));
    format!(
        "{dir}{}",
        tagged_name(name, &tag, PART_SUFFIX, remote_name_fits)
    )
}

/// 远程的文件名是否在长度上限以内（按字节计算）。
pub(crate) fn remote_name_fits(name: &str) -> bool {
    name.len() <= MAX_NAME_LEN
}

/// 本机的文件名是否在长度上限以内：Windows 按 UTF-16 单位计算（四字节的字符算两个），其他系统按字节计算。
pub(crate) fn local_name_fits(name: &str) -> bool {
    if cfg!(windows) {
        name.encode_utf16().count() <= MAX_NAME_LEN
    } else {
        name.len() <= MAX_NAME_LEN
    }
}

/// 下载到 `local` 时用的一次性临时文件"名称.<随机>.termlet-part"（命令行工具的单文件下载用）。
pub(crate) fn local_temp_part(local: &Path) -> PathBuf {
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tag = format!(".{}", random_hex(4));
    local.with_file_name(tagged_name(&name, &tag, PART_SUFFIX, local_name_fits))
}

/// 下载到 `local` 时的临时文件（与它在同一目录）。
pub(crate) fn local_part(local: &Path) -> PathBuf {
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    local.with_file_name(part_name(&name, 0, local_name_fits))
}

/// `path` 的第 `i` 个备选名称"名称 (i).扩展名"（与浏览器、资源管理器相同）；`i` 为 0 时就是它本身。
pub(crate) fn numbered(path: &Path, i: u32) -> PathBuf {
    if i == 0 {
        return path.to_path_buf();
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(numbered_name(&name, i))
}

/// 名称的第 `i` 个备选写法；扩展名取最后一个点之后的部分（`.bashrc` 这类以点开头的名称整个算作名称）。
pub(crate) fn numbered_name(name: &str, i: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({i}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({i})"),
    }
}

/// `n` 个随机字节的十六进制表示。
pub(crate) fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).expect("系统随机数不可用");
    data_encoding::HEXLOWER.encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        for (input, want) in [
            ("a:b?.txt", "a_b_.txt"),
            ("..", "_"),
            ("x. ", "x"),
            ("正常.log", "正常.log"),
            ("con", "_con"),
            ("NUL.tar.gz", "_NUL.tar.gz"),
            ("com1.log", "_com1.log"),
            ("console.log", "console.log"),
            ("a\u{1}b", "a_b"),
        ] {
            assert_eq!(safe_name(input), want, "{input}");
        }
        assert!(is_reserved_name("Lpt9.txt"));
        assert!(!is_reserved_name("lpt10"));
    }

    #[test]
    fn numbered_names() {
        assert_eq!(numbered_name("a.txt", 1), "a (1).txt");
        assert_eq!(numbered_name(".bashrc", 2), ".bashrc (2)");
        assert_eq!(numbered_name("a.tar.gz", 1), "a.tar (1).gz");
        assert_eq!(numbered_name("README", 3), "README (3)");
        assert_eq!(numbered(Path::new("x"), 0), PathBuf::from("x"));
    }

    // 接近上限的文件名改用短的临时文件名
    #[test]
    fn long_names_use_hashed_parts() {
        for name in [
            format!("{}.pdf", "a".repeat(246)),
            format!("{}.pdf", "文".repeat(82)),
        ] {
            let part = super::super::path::base_name(&remote_part(&format!("/d/{name}"), 3));
            assert!(part.len() <= MAX_NAME_LEN, "{part}");
            assert!(part.starts_with('.') && part.ends_with(&format!(".3{PART_SUFFIX}")));
            // 同一个文件总是同一个临时文件名
            assert_eq!(
                remote_part(&format!("/d/{name}"), 3),
                remote_part(&format!("/d/{name}"), 3)
            );
        }
        assert_eq!(remote_part("/d/a.txt", 0), format!("/d/a.txt{PART_SUFFIX}"));
        assert_eq!(
            remote_part("/d/a.txt", 2),
            format!("/d/a.txt.2{PART_SUFFIX}")
        );
        // Windows 按 UTF-16 单位：255 个汉字在 Windows 上放得下（765 字节）
        let han = "文".repeat(255);
        assert_eq!(local_name_fits(&han), cfg!(windows));
        let emoji = "😀".repeat(128);
        assert!(!local_name_fits(&emoji));
    }

    // 命令行工具的单文件上传不用"名称.termlet-part"
    #[test]
    fn cli_parts_are_one_off() {
        let a = remote_temp_part("/srv/a.txt");
        let b = remote_temp_part("/srv/a.txt");
        assert_ne!(a, remote_part("/srv/a.txt", 0));
        assert_ne!(a, b);
        assert!(
            a.starts_with("/srv/a.txt.") && a.ends_with(PART_SUFFIX),
            "{a}"
        );
    }
}
