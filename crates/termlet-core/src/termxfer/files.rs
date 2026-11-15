//! 本机文件：远端给的文件名在本机怎样用、重名时改名（绝不写入已有的文件）、列出要上传的文件。

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use encoding_rs::GBK;

use super::info::XferError;
use crate::sftp::is_reserved_name;

/// 本机文件名的长度上限（字节）。
const MAX_NAME_BYTES: usize = 255;

/// 把远端发来的文件名转为字符串：不是合法的 UTF-8 时按 GBK 解码（老系统、Windows 上的服务端）。
pub(crate) fn decode_name(raw: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(raw) {
        return s.to_string();
    }
    if let Some(s) = GBK.decode_without_bom_handling_and_without_replacement(raw) {
        return s.into_owned();
    }
    String::from_utf8_lossy(raw).replace('\u{fffd}', "_")
}

/// 把远端给的文件名变成本机安全的一级文件名：只取最后一级（防止 `../` 写到别处），
/// 去掉控制字符，Windows 不允许的字符换成 `_`，去掉结尾的点和空格，保留设备名前加 `_`，最长 255 字节。
pub(crate) fn safe_name(raw: &[u8]) -> String {
    let name = decode_name(raw);
    let last = name.rsplit(['/', '\\']).next().unwrap_or(&name);
    let mut out = String::with_capacity(last.len());
    for c in last.chars() {
        match c {
            c if (c as u32) < 0x20 || c == '\u{7f}' => {}
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => out.push('_'),
            c => out.push(c),
        }
    }
    let mut name = out.trim_end_matches(['.', ' ']).to_string();
    if name.is_empty() || name == "." || name == ".." {
        return "file".into();
    }
    if is_reserved_name(&name) {
        name.insert(0, '_');
    }
    while name.len() > MAX_NAME_BYTES {
        name.pop();
    }
    name
}

/// 名称的第 `i` 个备选写法"名称 (i).扩展名"（与浏览器、资源管理器相同）；`.bashrc` 这类以点开头的名称整个算作名称。
fn numbered_name(name: &str, i: u32) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{} ({i}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({i})"),
    }
}

/// 在 `dir` 中新建文件或目录 `name`，已有同名的文件或目录时改为"名称 (1).扩展名"。
/// 原子地新建（文件用 `create_new`，目录用 `create_dir`）：检查之后被别人抢先建了同名的就换下一个名称，
/// 绝不写入已有的文件或目录。新建文件时返回打开的文件，目录时为 `None`。
pub(crate) fn create_unique(
    dir: &Path,
    name: &str,
    is_dir: bool,
) -> Result<(PathBuf, Option<File>), XferError> {
    for i in 0..10000 {
        let path = if i == 0 {
            dir.join(name)
        } else {
            dir.join(numbered_name(name, i))
        };
        if fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        let created = if is_dir {
            make_dir(&path).map(|_| None)
        } else {
            create_new_file(&path).map(Some)
        };
        match created {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(XferError::local(&path, &e)),
        }
    }
    Err(XferError::NoUniqueName(name.to_string()))
}

/// 新建一个文件（已存在时失败，不覆盖）。
pub(crate) fn create_new_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    options.open(path)
}

/// 新建一个目录（已存在时失败）。
fn make_dir(path: &Path) -> io::Result<()> {
    dir_builder(false).create(path)
}

/// 新建目录及其上级（已存在时不报错）。
pub(crate) fn make_dir_all(path: &Path) -> io::Result<()> {
    dir_builder(true).create(path)
}

fn dir_builder(recursive: bool) -> fs::DirBuilder {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o750);
    }
    builder
}

/// 要上传的一项（文件或目录）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Source {
    /// 本机完整路径
    pub abs: PathBuf,
    /// 从所选的那一项开始的相对路径（第一项是所选文件或目录的名称）
    pub rel: Vec<String>,
    pub is_dir: bool,
    pub size: u64,
    pub perm: u32,
    /// 所选的第几项（trzsz 的 `path_id`）
    pub id: usize,
}

impl Source {
    /// 最后一级名称。
    pub(crate) fn name(&self) -> &str {
        self.rel.last().map(String::as_str).unwrap_or_default()
    }
}

/// [`collect`] 的失败原因：选了目录但不允许，或读不了。
#[derive(Debug)]
pub(crate) enum CollectError {
    Directory(PathBuf),
    Other(XferError),
}

/// 列出要上传的文件；`allow_dirs` 时目录展开为其中的全部文件与子目录（目录本身也是一项，空目录也能传）。
/// 所选目录中没有读权限的子目录不展开（它本身照常建出来），记在返回的跳过列表中（从所选的那一项开始的相对路径）；
/// 所选的目录本身读不了时照常报错。所选目录是指向目录的链接（Windows 的 junction、符号链接）时展开它指向的目录，
/// 子项中的链接不跟随（避免循环）、连同设备文件等一起略过。
pub(crate) fn collect(
    paths: &[PathBuf],
    allow_dirs: bool,
) -> Result<(Vec<Source>, Vec<String>), CollectError> {
    let mut out = Vec::new();
    let mut skipped = Vec::new();
    for (id, path) in paths.iter().enumerate() {
        let abs = std::path::absolute(path)
            .map_err(|e| CollectError::Other(XferError::local(path, &e)))?;
        let meta =
            fs::metadata(&abs).map_err(|e| CollectError::Other(XferError::local(&abs, &e)))?;
        let name = file_name(&abs);
        if !meta.is_dir() {
            if !meta.is_file() {
                return Err(CollectError::Other(XferError::NotRegularFile(abs)));
            }
            out.push(Source {
                rel: vec![name],
                size: meta.len(),
                perm: perm_of(&meta),
                is_dir: false,
                abs,
                id,
            });
            continue;
        }
        if !allow_dirs {
            return Err(CollectError::Directory(abs));
        }
        let mut rel = vec![name];
        walk(&abs, &meta, &mut rel, id, true, &mut out, &mut skipped)
            .map_err(CollectError::Other)?;
    }
    Ok((out, skipped))
}

/// 加入目录 `dir` 本身与其中的全部内容（按名称排序，深度优先，与 Go 的 `filepath.WalkDir` 相同的顺序）。
fn walk(
    dir: &Path,
    meta: &fs::Metadata,
    rel: &mut Vec<String>,
    id: usize,
    root: bool,
    out: &mut Vec<Source>,
    skipped: &mut Vec<String>,
) -> Result<(), XferError> {
    out.push(Source {
        abs: dir.to_path_buf(),
        rel: rel.clone(),
        is_dir: true,
        size: 0,
        perm: perm_of(meta),
        id,
    });
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if !root && e.kind() == io::ErrorKind::PermissionDenied => {
            skipped.push(rel.join("/"));
            return Ok(());
        }
        Err(e) => return Err(XferError::local(dir, &e)),
    };
    let mut entries: Vec<fs::DirEntry> = entries
        .collect::<Result<_, _>>()
        .map_err(|e| XferError::local(dir, &e))?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let kind = entry.file_type().map_err(|e| XferError::local(&path, &e))?;
        // 符号链接、junction、设备文件等跳过
        if kind.is_symlink() || !(kind.is_dir() || kind.is_file()) {
            continue;
        }
        let meta = entry.metadata().map_err(|e| XferError::local(&path, &e))?;
        rel.push(entry.file_name().to_string_lossy().into_owned());
        if kind.is_dir() {
            walk(&path, &meta, rel, id, false, out, skipped)?;
        } else {
            out.push(Source {
                abs: path,
                rel: rel.clone(),
                is_dir: false,
                size: meta.len(),
                perm: perm_of(&meta),
                id,
            });
        }
        rel.pop();
    }
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// 权限位（trzsz 传目录时告诉服务器）：Unix 取文件的权限；Windows 与 Go 相同，只读的为 0444，否则 0666，目录再加执行位。
fn perm_of(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    }
    #[cfg(not(unix))]
    {
        let base = if meta.permissions().readonly() {
            0o444
        } else {
            0o666
        };
        if meta.is_dir() { base | 0o111 } else { base }
    }
}
