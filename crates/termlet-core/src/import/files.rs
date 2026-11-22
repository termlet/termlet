//! 读取来源文件：本机目录、路径展开、大小限制与编码识别。

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use encoding_rs::Encoding;

use super::{ImportError, sys};

/// 单个配置文件的大小上限，防止误选超大文件耗尽内存。
pub(super) const MAX_CONFIG_SIZE: u64 = 16 << 20;

/// 解析时用到的本机环境。测试中直接构造，与运行环境无关。
#[derive(Clone, Debug)]
pub(super) struct LocalEnv {
    pub home: Option<PathBuf>,
    /// "文档"文件夹（可能被重定向到其他磁盘或 OneDrive）
    pub documents: Option<PathBuf>,
    /// "下载"与"桌面"文件夹：找其他程序导出的文件（Netcatty 的 CSV）
    pub downloads: Option<PathBuf>,
    pub desktop: Option<PathBuf>,
    /// 系统的配置目录（Windows 为 `%APPDATA%`）
    pub config_dir: Option<PathBuf>,
    /// 系统的数据目录（Windows 为 `%APPDATA%`，macOS 为 `~/Library/Application Support`，Linux 为
    /// `~/.local/share`）
    pub data_dir: Option<PathBuf>,
    /// 系统盘的盘符字母（不含冒号）
    pub system_drive: String,
    /// 系统 ANSI 代码页对应的编码：MobaXterm 等老程序按它保存配置文件（中文 Windows 上为 GBK）
    pub ansi: &'static Encoding,
    /// Termora 的数据目录（环境变量 `TERMORA_BASE_DATA_DIR`）
    pub termora_dir: Option<PathBuf>,
    /// ShellRS 的数据目录（环境变量 `SHELLRS_DATA_DIR`）
    pub shellrs_dir: Option<PathBuf>,
}

impl LocalEnv {
    pub fn detect() -> Self {
        let home = dirs::home_dir();
        let documents = dirs::document_dir().or_else(|| home.as_ref().map(|h| h.join("Documents")));
        let system_drive = std::env::var("SystemDrive")
            .ok()
            .and_then(|d| drive_letter(&d))
            .unwrap_or_else(|| "C".into());
        Self {
            downloads: dirs::download_dir().or_else(|| home.as_ref().map(|h| h.join("Downloads"))),
            desktop: dirs::desktop_dir().or_else(|| home.as_ref().map(|h| h.join("Desktop"))),
            home,
            documents,
            config_dir: dirs::config_dir(),
            data_dir: dirs::data_dir(),
            system_drive,
            ansi: sys::ansi_code_page()
                .map(encoding_for_code_page)
                .unwrap_or(encoding_rs::GB18030),
            termora_dir: std::env::var_os("TERMORA_BASE_DATA_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            shellrs_dir: std::env::var_os("SHELLRS_DATA_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
        }
    }

    /// 展开路径开头的 `~`。
    pub fn expand_home(&self, path: &str) -> String {
        let rest = if path == "~" {
            ""
        } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
            rest
        } else {
            return path.to_string();
        };
        let Some(home) = &self.home else {
            return path.to_string();
        };
        if rest.is_empty() {
            return home.to_string_lossy().into_owned();
        }
        let rest = if cfg!(windows) {
            rest.replace('/', "\\")
        } else {
            rest.to_string()
        };
        home.join(rest).to_string_lossy().into_owned()
    }

    /// 用户选择的路径：去掉首尾空白，展开 `~`。
    pub fn input_path(&self, path: &Path) -> PathBuf {
        match path.to_str() {
            Some(s) => PathBuf::from(self.expand_home(s.trim())),
            None => path.to_path_buf(),
        }
    }
}

/// “下载”“桌面”“文档”中修改时间最新的、名称（小写）满足 `matches` 的文件：其他程序导出的文件
/// （Netcatty 的 CSV、NyaTerm 的快捷命令）多半在这几处。都没有时为 `None`。
pub(super) fn newest_export(env: &LocalEnv, matches: impl Fn(&str) -> bool) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for dir in [&env.downloads, &env.desktop, &env.documents]
        .into_iter()
        .flatten()
    {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if !matches(&entry.file_name().to_string_lossy().to_lowercase()) {
                continue;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            if meta.is_file() && best.as_ref().is_none_or(|(t, _)| modified > *t) {
                best = Some((modified, entry.path()));
            }
        }
    }
    best.map(|(_, path)| path)
}

/// 从 "C:" 形式的卷名中取出盘符字母；不是盘符（例如 UNC 路径）时为 `None`。
pub(super) fn drive_letter(volume: &str) -> Option<String> {
    let bytes = volume.as_bytes();
    (bytes.len() == 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
        .then(|| volume[..1].to_string())
}

/// 读取单个配置文件；`path` 是目录时读取其中名为 `name` 的文件（例如便携版的程序目录）。
/// 返回内容与实际读取的文件。
pub(super) fn read_config_file(
    path: &Path,
    name: &str,
    env: &LocalEnv,
) -> Result<(Vec<u8>, PathBuf), ImportError> {
    let mut path = env.input_path(path);
    if path.is_dir() {
        path = path.join(name);
    }
    let raw = read_limited(&path, MAX_CONFIG_SIZE)?;
    Ok((raw, path))
}

/// 读取文件，超过 `limit` 字节时报错。
pub(super) fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>, ImportError> {
    let io = |source| ImportError::Io {
        path: path.to_path_buf(),
        source,
    };
    let file = fs::File::open(path).map_err(io)?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "不是普通文件",
        )));
    }
    let too_large = || ImportError::TooLarge {
        path: path.to_path_buf(),
        limit_mb: limit >> 20,
    };
    if meta.len() > limit {
        return Err(too_large());
    }
    let mut raw = Vec::with_capacity(meta.len() as usize);
    file.take(limit + 1).read_to_end(&mut raw).map_err(io)?;
    if raw.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(raw)
}

/// 把配置文件内容转为文本：识别 UTF-16（带 BOM）与 UTF-8（可带 BOM），都不是时按 `ansi` 解码。
/// 无法解码的字节换成替换字符，不会失败。
pub(super) fn decode_text(raw: &[u8], ansi: &'static Encoding) -> String {
    if raw.starts_with(&[0xFF, 0xFE]) || raw.starts_with(&[0xFE, 0xFF]) {
        // decode 按 BOM 选择字节序并去掉 BOM
        return encoding_rs::UTF_16LE.decode(raw).0.into_owned();
    }
    let raw = raw.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(raw);
    match std::str::from_utf8(raw) {
        Ok(text) => text.to_string(),
        Err(_) => ansi.decode_without_bom_handling(raw).0.into_owned(),
    }
}

/// Windows 代码页对应的编码；不认识的代码页按 GB18030（GBK 的超集）处理。
pub(super) fn encoding_for_code_page(code_page: u32) -> &'static Encoding {
    use encoding_rs::*;
    match code_page {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        949 => EUC_KR,
        950 => BIG5,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1252 => WINDOWS_1252,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        _ => GB18030, // 936 及其他
    }
}

/// 两个路径是否是同一个位置（Windows 上不区分大小写）。
pub(super) fn same_path(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// 拆分后的相对路径中的目录部分（不含文件名），作为分组的各级名称。
pub(super) fn dir_segments(rel: &[String]) -> &[String] {
    &rel[..rel.len().saturating_sub(1)]
}

/// 遍历目录中扩展名为 `ext`（不区分大小写）的文件，按名称排序、深度优先，返回相对路径的各级名称与文件路径。
/// 不跟随符号链接与目录联接，防止被引到目录外；读不了的子目录跳过。
pub(super) fn walk_files(
    root: &Path,
    ext: &str,
) -> Result<Vec<(Vec<String>, PathBuf)>, ImportError> {
    /// 目录层数上限，防止异常的目录结构
    const MAX_DEPTH: usize = 32;
    let mut out = Vec::new();
    let entries = read_dir_sorted(root).map_err(|source| ImportError::Io {
        path: root.to_path_buf(),
        source,
    })?;
    let mut stack: Vec<(Vec<String>, std::vec::IntoIter<fs::DirEntry>)> =
        vec![(Vec::new(), entries.into_iter())];
    while let Some((prefix, iter)) = stack.last_mut() {
        let Some(entry) = iter.next() else {
            stack.pop();
            continue;
        };
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let mut rel = prefix.clone();
        rel.push(name);
        if file_type.is_dir() {
            if rel.len() < MAX_DEPTH
                && let Ok(children) = read_dir_sorted(&entry.path())
            {
                stack.push((rel, children.into_iter()));
            }
        } else if file_type.is_file()
            && Path::new(rel.last().map_or("", |s| s.as_str()))
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            out.push((rel, entry.path()));
        }
    }
    Ok(out)
}

fn read_dir_sorted(dir: &Path) -> std::io::Result<Vec<fs::DirEntry>> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().into_owned());
    Ok(entries)
}

/// 数据库在临时目录中的副本（含 WAL 文件），丢弃时删除：来源的程序运行时也在使用它，复制一份再读，不加锁、不影响它。
/// 副本中可能有来源的全部凭据，临时目录只允许当前用户访问。
pub(super) struct TempCopy {
    pub(super) dir: PathBuf,
    name: std::ffi::OsString,
}

impl TempCopy {
    /// 复制 `src`（与旁边的 `-wal` 文件），超过 `limit` 字节时报错。
    pub(super) fn of(src: &Path, limit: u64) -> Result<Self, ImportError> {
        let dir =
            std::env::temp_dir().join(format!("termlet-import-{}", crate::id::random_token(8)));
        fs::create_dir(&dir).map_err(|source| ImportError::Io {
            path: dir.clone(),
            source,
        })?;
        let name = src
            .file_name()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "data.db".into());
        let copy = Self { dir, name };
        if let Err(e) = crate::platform::restrict_dir(&copy.dir) {
            tracing::warn!(error = %e, "收紧导入用临时目录的权限失败");
        }
        copy_limited(src, &copy.file(), limit)?;
        let mut wal = src.as_os_str().to_owned();
        wal.push("-wal");
        let wal = PathBuf::from(wal);
        if wal.is_file() {
            let mut wal_copy = copy.file().into_os_string();
            wal_copy.push("-wal");
            copy_limited(&wal, Path::new(&wal_copy), limit)?;
        }
        Ok(copy)
    }

    /// 副本的路径（文件名与来源相同）。
    pub(super) fn file(&self) -> PathBuf {
        self.dir.join(&self.name)
    }
}

impl Drop for TempCopy {
    fn drop(&mut self) {
        if let Err(e) = fs::remove_dir_all(&self.dir) {
            tracing::warn!(error = %e, dir = %self.dir.display(), "删除导入用的临时文件失败");
        }
    }
}

fn copy_limited(src: &Path, dst: &Path, limit: u64) -> Result<(), ImportError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| ImportError::Io { path, source }
    };
    let input = fs::File::open(src).map_err(io(src))?;
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)
        .map_err(io(dst))?;
    let n = std::io::copy(&mut input.take(limit + 1), &mut out).map_err(io(src))?;
    if n > limit {
        return Err(ImportError::TooLarge {
            path: src.to_path_buf(),
            limit_mb: limit >> 20,
        });
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// 测试用的本机环境：固定的用户目录与中文 Windows 的代码页。
    pub fn env() -> LocalEnv {
        LocalEnv {
            home: Some(PathBuf::from(if cfg!(windows) {
                r"C:\Users\me"
            } else {
                "/home/me"
            })),
            documents: None,
            downloads: None,
            desktop: None,
            config_dir: None,
            data_dir: None,
            system_drive: "C".into(),
            ansi: encoding_for_code_page(936),
            termora_dir: None,
            shellrs_dir: None,
        }
    }

    pub fn utf16le(s: &str) -> Vec<u8> {
        let mut out = vec![0xFF, 0xFE];
        for unit in s.encode_utf16() {
            out.extend_from_slice(&unit.to_le_bytes());
        }
        out
    }

    pub fn write(path: &Path, data: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, data).unwrap();
    }

    #[test]
    fn decode_text_detects_encodings() {
        let gbk = encoding_rs::GBK.encode("中文=1").0.into_owned();
        let mut utf16be = vec![0xFE, 0xFF];
        for unit in "中文=1".encode_utf16() {
            utf16be.extend_from_slice(&unit.to_be_bytes());
        }
        let cases: [(&str, Vec<u8>); 5] = [
            ("UTF-8", "中文=1".as_bytes().to_vec()),
            (
                "UTF-8 BOM",
                [&[0xEF, 0xBB, 0xBF][..], "中文=1".as_bytes()].concat(),
            ),
            ("UTF-16 BOM", utf16le("中文=1")),
            ("UTF-16BE BOM", utf16be),
            ("GBK", gbk),
        ];
        for (name, raw) in cases {
            assert_eq!(
                decode_text(&raw, encoding_for_code_page(936)),
                "中文=1",
                "{name}"
            );
        }
        // 其他代码页
        let big5 = encoding_rs::BIG5.encode("中文").0.into_owned();
        assert_eq!(decode_text(&big5, encoding_for_code_page(950)), "中文");
    }

    #[test]
    fn expand_home_and_drive_letter() {
        let env = env();
        let home = env.home.clone().unwrap();
        assert_eq!(env.expand_home("~"), home.to_string_lossy());
        let expanded = env.expand_home("~/.ssh/id_rsa");
        assert!(expanded.starts_with(&*home.to_string_lossy()));
        assert!(expanded.ends_with("id_rsa") && !expanded.contains('~'));
        assert_eq!(env.expand_home("~user/x"), "~user/x");
        assert_eq!(env.expand_home("/abs"), "/abs");
        assert_eq!(drive_letter("D:").as_deref(), Some("D"));
        assert_eq!(drive_letter(r"\\server\share"), None);
    }

    #[test]
    fn read_config_file_limits_size_and_accepts_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("MobaXterm.ini"), b"x=1");
        let (raw, file) = read_config_file(dir.path(), "MobaXterm.ini", &env()).unwrap();
        assert_eq!(raw, b"x=1");
        assert_eq!(file, dir.path().join("MobaXterm.ini"));
        assert!(matches!(
            read_limited(&dir.path().join("MobaXterm.ini"), 2),
            Err(ImportError::TooLarge { .. })
        ));
        assert!(matches!(
            read_config_file(&dir.path().join("missing"), "x", &env()),
            Err(ImportError::Io { .. })
        ));
    }

    #[test]
    fn walk_files_sorts_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["b.XSH", "a/c.xsh", "a/readme.txt", "生产/d.xsh"] {
            write(&dir.path().join(name), b"");
        }
        let files = walk_files(dir.path(), "xsh").unwrap();
        let rel: Vec<String> = files.iter().map(|(r, _)| r.join("/")).collect();
        assert_eq!(rel, ["a/c.xsh", "b.XSH", "生产/d.xsh"]);
        assert!(walk_files(&dir.path().join("missing"), "xsh").is_err());
    }
}
