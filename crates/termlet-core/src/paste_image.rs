//! 终端中粘贴截图：剪贴板中只有图片时，把它存成文件、把路径填进输入行（给在终端中运行的 Claude Code 等读取）。
//!
//! - 本地终端存到本机临时目录下的 `termlet-pastes`（[`save_local`]）；SSH 终端经这个终端的连接用 SFTP 写到
//!   服务器上只有自己可访问的目录（[`upload`]）。两边都只保留最近 20 张、7 天以内的，各自清理。
//! - 图片最大 8 MB；接受 PNG、JPEG、GIF、WebP（按文件头识别，见 [`check`]）。
//! - 填进终端的文字见 [`local_path_text`]、[`remote_path_text`]：路径中有空白或 shell 特殊字符时加引号。
//!
//! 用法（界面）：粘贴时剪贴板中有文字就按文字粘贴；只有图片时取出图片字节（BMP 等先转成 PNG），
//! 本地终端在后台执行器上调用 [`save_local`]（目录用 [`local_dir`]），SSH 终端在核心的运行时上调用 [`upload`]
//! （"只用一个通道"的主机、Telnet、串口不支持）；得到的文字按粘贴写进这个终端（同步输入时也只写这一个）。

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::NaiveDateTime;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::AsyncWriteExt;

use crate::ssh::{Connection, SshError};
use crate::{CoreError, Invalid, Result, platform};

/// 截图的大小上限。
pub const MAX_IMAGE_BYTES: usize = 8 << 20;
/// 保留最近的这么多张。
const KEEP_FILES: usize = 20;
/// 保留这么久以内的。
const KEEP_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// 截图文件名的前缀：`termlet-paste-<YYYYMMDD-HHMMSS>-<8 位随机十六进制>.<扩展名>`（本机时间）。
const PREFIX: &str = "termlet-paste-";
const NAME_TIME: &str = "%Y%m%d-%H%M%S";
/// 文件名中时间的长度（`20261014-210500`）。
const NAME_TIME_LEN: usize = 15;
/// 服务器上的保存位置（相对主目录）。
const REMOTE_DIR: &str = ".cache/termlet/paste";
/// 上传的总时限（打开 SFTP、建目录、写入、清理）。
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// 图片格式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageFormat {
    const ALL: [ImageFormat; 4] = [Self::Png, Self::Jpeg, Self::Gif, Self::Webp];

    /// 按文件头识别。
    pub fn detect(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some(Self::Jpeg)
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            Some(Self::Webp)
        } else {
            None
        }
    }

    /// 文件的扩展名（不含点）。
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }
}

/// 检查要粘贴的图片：不超过 [`MAX_IMAGE_BYTES`]（`Invalid { field: "image", reason: "too_large" }`），
/// 是认得的格式（`reason: "unsupported_format"`）。[`save_local`]、[`upload`] 都会先检查，
/// 界面可以在读剪贴板后先调用，尽早提示。
pub fn check(image: &[u8]) -> Result<ImageFormat> {
    if image.len() > MAX_IMAGE_BYTES {
        return Err(Invalid::new("image", "too_large").into());
    }
    ImageFormat::detect(image).ok_or_else(|| Invalid::new("image", "unsupported_format").into())
}

/// 本地终端的截图目录：本机临时目录下的 `termlet-pastes`。
pub fn local_dir() -> PathBuf {
    std::env::temp_dir().join("termlet-pastes")
}

/// 本地终端：把图片存到 `dir`（不存在时创建，只有当前用户可访问），清理旧的截图，返回文件路径。
/// 同步的文件操作，界面在后台执行器上调用。
pub fn save_local(dir: &Path, image: &[u8]) -> Result<PathBuf> {
    let format = check(image)?;
    // 每次都收紧：Unix 上临时目录是共享的，别人事先建好的同名目录改不了权限，就不往里写
    platform::ensure_private_dir(dir).map_err(|e| {
        context(
            e,
            format!("无法创建只有自己可访问的截图目录 {}", dir.display()),
        )
    })?;
    let name = file_name(now_local(), &random_hex(), format);
    let path = dir.join(&name);
    write_new(&path, image).map_err(|e| context(e, format!("保存截图 {} 失败", path.display())))?;
    prune_local(dir, &name, SystemTime::now());
    tracing::info!(path = %path.display(), bytes = image.len(), "截图已保存到本机");
    Ok(path)
}

/// 新建文件写入（已存在时失败），写不完时删除。
fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    if let Err(e) = file.write_all(data).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(e);
    }
    Ok(())
}

/// 只保留最近的 20 张（按修改时间，刚写入的 `current` 算最新的一张）、7 天以内的截图；其他文件不动。尽力而为。
fn prune_local(dir: &Path, current: &str, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(bool, SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !is_paste_name(&name) {
                return None;
            }
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            Some((name == current, modified, entry.path()))
        })
        .collect();
    files.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    for (i, (is_current, modified, path)) in files.iter().enumerate() {
        if *is_current {
            continue;
        }
        let expired = now
            .duration_since(*modified)
            .is_ok_and(|age| age > KEEP_AGE);
        if i >= KEEP_FILES || expired {
            let _ = fs::remove_file(path);
        }
    }
}

/// 插入本地终端时路径的写法。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathStyle {
    /// PowerShell、命令提示符：原样，有空白时加双引号
    Windows,
    /// WSL：转成 `/mnt/<盘符>/…`，需要时加单引号
    Wsl,
    /// Git Bash、MSYS2、Cygwin 与 Unix 上的 shell：Windows 上把 `\` 换成 `/`，需要时加单引号
    Posix,
}

/// 本地终端中填进输入行的文字。
pub fn local_path_text(path: &Path, style: PathStyle) -> String {
    let text = path.to_string_lossy();
    match style {
        PathStyle::Windows if text.chars().any(char::is_whitespace) => format!("\"{text}\""),
        PathStyle::Windows => text.into_owned(),
        PathStyle::Wsl => shell_quote(&wsl_path(&text)),
        PathStyle::Posix if cfg!(windows) => shell_quote(&text.replace('\\', "/")),
        PathStyle::Posix => shell_quote(&text),
    }
}

/// 远程终端中填进输入行的文字（[`upload`] 返回的路径）。
pub fn remote_path_text(path: &str) -> String {
    shell_quote(path)
}

/// Windows 路径在 WSL 中的位置：`C:\a\b` → `/mnt/c/a/b`。
fn wsl_path(path: &str) -> String {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
    {
        let drive = (bytes[0] as char).to_ascii_lowercase();
        return format!("/mnt/{drive}/{}", path[3..].replace('\\', "/"));
    }
    path.replace('\\', "/")
}

/// 只有安全的字符时原样，否则用单引号括起（其中的单引号写成 `'"'"'`）。
fn shell_quote(text: &str) -> String {
    let safe = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./~-".contains(&b));
    if safe {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r#"'"'"'"#))
    }
}

/// SSH 终端：经这个终端的连接打开 SFTP（不再认证），把图片写到服务器上只有自己可访问的目录，返回绝对路径。
///
/// 首选 `~/.cache/termlet/paste`（目录 700、文件 600，只保留最近 20 张、7 天以内的）；用户目录不可用
/// （没有用户目录、只读、配额已满）时在 `/tmp` 下新建一个私有目录。服务器不允许修改权限时删除已写的内容并报错，
/// 不留下其他用户可读的截图。最多用 30 秒。
pub async fn upload(connection: &Connection, image: &[u8]) -> Result<String> {
    let format = check(image)?;
    tracing::debug!(host = %connection.label(), bytes = image.len(), format = ?format, "上传粘贴的截图");
    let work = async {
        let sftp = open_sftp(connection).await?;
        let result = write_paste(&sftp, image, format, now_local(), &random_hex()).await;
        let _ = sftp.0.close().await;
        result
    };
    match tokio::time::timeout(UPLOAD_TIMEOUT, work).await {
        Ok(result) => {
            match &result {
                Ok(path) => {
                    tracing::info!(host = %connection.label(), path = %path, "截图已上传到服务器")
                }
                Err(e) => tracing::warn!(host = %connection.label(), "上传截图失败：{e}"),
            }
            result
        }
        Err(_) => Err(CoreError::Io(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("上传截图在 {} 秒内没有完成", UPLOAD_TIMEOUT.as_secs()),
        ))),
    }
}

async fn open_sftp(connection: &Connection) -> Result<Sftp> {
    let channel_error =
        |e: russh::Error| CoreError::Io(io::Error::other(SshError::Channel(e.to_string())));
    let channel = connection
        .handle()
        .channel_open_session()
        .await
        .map_err(channel_error)?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(channel_error)?;
    let session = SftpSession::new(channel.into_stream()).await.map_err(|e| {
        CoreError::Io(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("服务器不支持 SFTP：{e}"),
        ))
    })?;
    Ok(Sftp(session))
}

/// 加上说明的 I/O 错误（保留错误种类，界面据此选用文案）。
fn context(error: io::Error, what: String) -> CoreError {
    CoreError::Io(io::Error::new(error.kind(), format!("{what}：{error}")))
}

fn denied(what: String) -> CoreError {
    CoreError::Io(io::Error::new(io::ErrorKind::PermissionDenied, what))
}

fn now_local() -> NaiveDateTime {
    chrono::Local::now().naive_local()
}

fn random_hex() -> String {
    let mut bytes = [0u8; 4];
    getrandom::fill(&mut bytes).expect("系统随机数不可用");
    data_encoding::HEXLOWER.encode(&bytes)
}

fn file_name(now: NaiveDateTime, random: &str, format: ImageFormat) -> String {
    format!(
        "{PREFIX}{}-{random}.{}",
        now.format(NAME_TIME),
        format.extension()
    )
}

fn is_paste_name(name: &str) -> bool {
    name.starts_with(PREFIX)
        && ImageFormat::ALL.iter().any(|f| {
            name.strip_suffix(f.extension())
                .is_some_and(|rest| rest.ends_with('.'))
        })
}

/// 按文件名中的时间（写入时本机的时间）判断截图是否超过 7 天；认不出时间的不算过期（只按张数清理）。
fn expired(name: &str, now: NaiveDateTime) -> bool {
    let Some(stamp) = name
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.get(..NAME_TIME_LEN))
    else {
        return false;
    };
    NaiveDateTime::parse_from_str(stamp, NAME_TIME)
        .is_ok_and(|at| (now - at).to_std().is_ok_and(|age| age > KEEP_AGE))
}

/// 远端文件的种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dir,
    File,
    Symlink,
    Other,
}

/// 远端文件的信息：种类与权限位。
#[derive(Clone, Copy, Debug)]
struct Meta {
    kind: Kind,
    perm: u32,
}

/// 写入截图用到的远端文件操作（平时是 SFTP，测试中换成内存中的文件系统）。路径都是 `/` 分隔的绝对路径。
trait RemoteFs {
    type File;
    /// 登录用户的主目录（SFTP 的当前目录）
    async fn home(&self) -> io::Result<String>;
    /// 跟随符号链接；不存在时为 `None`
    async fn stat(&self, path: &str) -> io::Result<Option<Meta>>;
    /// 不跟随符号链接；不存在时为 `None`
    async fn lstat(&self, path: &str) -> io::Result<Option<Meta>>;
    /// 新建目录，已存在时失败
    async fn mkdir(&self, path: &str) -> io::Result<()>;
    async fn chmod(&self, path: &str, mode: u32) -> io::Result<()>;
    /// 目录中的条目：名称与种类（不跟随符号链接）
    async fn list(&self, dir: &str) -> io::Result<Vec<(String, Kind)>>;
    /// 新建文件，已存在（包括符号链接）时失败
    async fn create_new(&self, path: &str) -> io::Result<Self::File>;
    async fn file_chmod(&self, file: &mut Self::File, mode: u32) -> io::Result<()>;
    async fn file_write(&self, file: &mut Self::File, data: &[u8]) -> io::Result<()>;
    async fn file_close(&self, file: Self::File) -> io::Result<()>;
    async fn remove_file(&self, path: &str) -> io::Result<()>;
    async fn remove_dir(&self, path: &str) -> io::Result<()>;
}

/// 写入截图，返回服务器上的绝对路径。
///
/// 文件放在只有自己可访问的目录里：有的 SFTP 服务器新建文件时不理会请求的权限（按 0666 与 umask 创建，通常是 644），
/// 之后再改为 600 之前，放在共享的 `/tmp` 中的文件其他用户可以读取。
async fn write_paste<F: RemoteFs>(
    fs: &F,
    image: &[u8],
    format: ImageFormat,
    now: NaiveDateTime,
    random: &str,
) -> Result<String> {
    let dir = paste_dir(fs, random).await?;
    let name = file_name(now, random, format);
    let path = format!("{dir}/{name}");
    let mut file = fs
        .create_new(&path)
        .await
        .map_err(|e| context(e, format!("无法在服务器上创建 {path}")))?;
    if let Err(e) = fs.file_chmod(&mut file, 0o600).await {
        let _ = fs.file_close(file).await;
        let _ = fs.remove_file(&path).await;
        return Err(denied(format!(
            "服务器不允许把截图设为只有自己可读，未粘贴：{e}"
        )));
    }
    if let Err(e) = fs.file_write(&mut file, image).await {
        let _ = fs.file_close(file).await;
        let _ = fs.remove_file(&path).await;
        return Err(context(e, "写入截图失败".into()));
    }
    if let Err(e) = fs.file_close(file).await {
        let _ = fs.remove_file(&path).await;
        return Err(context(e, "写入截图失败".into()));
    }
    if !dir.starts_with("/tmp/") {
        prune_remote(fs, &dir, &name, now).await;
    }
    Ok(path)
}

/// 只有自己可访问的目录：`~/.cache/termlet/paste`，不可用时是 `/tmp` 下新建的目录。
async fn paste_dir<F: RemoteFs>(fs: &F, random: &str) -> Result<String> {
    if let Ok(home) = fs.home().await
        && home.starts_with('/')
        && home != "/"
    {
        let dir = format!("{}/{REMOTE_DIR}", home.trim_end_matches('/'));
        if mkdir_all(fs, &dir).await.is_ok() && private_dir(fs, &dir).await.is_ok() {
            return Ok(dir);
        }
    }
    // 新建而不是沿用：/tmp 中同名的目录可能是别人事先建好的；mkdir 在已存在时失败
    let dir = format!("/tmp/{PREFIX}{random}");
    fs.mkdir(&dir)
        .await
        .map_err(|e| context(e, format!("无法在服务器上创建 {dir}")))?;
    if let Err(e) = private_dir(fs, &dir).await {
        let _ = fs.remove_dir(&dir).await;
        return Err(e);
    }
    Ok(dir)
}

/// 逐级创建目录（已存在的跳过，跟随符号链接）。
async fn mkdir_all<F: RemoteFs>(fs: &F, dir: &str) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = dir;
    loop {
        match fs.stat(current).await? {
            Some(meta) if meta.kind == Kind::Dir => break,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{current} 不是目录"),
                ));
            }
            None => {
                missing.push(current);
                match current.rsplit_once('/') {
                    Some(("", _)) | None => break,
                    Some((parent, _)) => current = parent,
                }
            }
        }
    }
    for path in missing.into_iter().rev() {
        if let Err(e) = fs.mkdir(path).await {
            // 同时有别人建好了
            if !matches!(fs.stat(path).await, Ok(Some(meta)) if meta.kind == Kind::Dir) {
                return Err(e);
            }
        }
    }
    Ok(())
}

/// 把目录设为只有自己可访问（700），并确认确实如此：不是符号链接，组与其他用户没有任何权限。
/// 只有属主（或 root）能修改权限，修改成功也说明目录属于自己。
async fn private_dir<F: RemoteFs>(fs: &F, dir: &str) -> Result<()> {
    fs.chmod(dir, 0o700)
        .await
        .map_err(|e| denied(format!("服务器不允许把 {dir} 设为只有自己可访问：{e}")))?;
    let meta = fs
        .lstat(dir)
        .await
        .map_err(|e| context(e, format!("读取 {dir} 的权限失败")))?;
    match meta {
        Some(meta) if meta.kind == Kind::Dir && meta.perm & 0o077 == 0 => Ok(()),
        Some(meta) => Err(denied(format!(
            "{dir} 不是只有自己可访问的目录（{:?}，{:o}）",
            meta.kind, meta.perm
        ))),
        None => Err(denied(format!("{dir} 不存在"))),
    }
}

/// 删除以前粘贴的截图中超过 7 天的，以及最近 20 张以外的（尽力而为，失败不影响这次粘贴）。
/// `current` 是刚写入的这张，算最新的、不删除。是否过期按文件名中的本机时间判断：服务器上的修改时间来自
/// 服务器的时钟，与本机相差 7 天以上时（服务器时钟落后）刚写入的截图也会被当作过期。
async fn prune_remote<F: RemoteFs>(fs: &F, dir: &str, current: &str, now: NaiveDateTime) {
    let Ok(entries) = fs.list(dir).await else {
        return;
    };
    let mut names: Vec<String> = entries
        .into_iter()
        .filter(|(name, kind)| *kind == Kind::File && is_paste_name(name))
        .map(|(name, _)| name)
        .collect();
    // 文件名以时间开头：按名称从新到旧
    names.sort_by(|a, b| (b == current).cmp(&(a == current)).then(b.cmp(a)));
    for (i, name) in names.iter().enumerate() {
        if name == current {
            continue;
        }
        if i >= KEEP_FILES || expired(name, now) {
            let _ = fs.remove_file(&format!("{dir}/{name}")).await;
        }
    }
}

/// 经 SFTP 访问服务器。
struct Sftp(SftpSession);

fn sftp_error(error: russh_sftp::client::error::Error) -> io::Error {
    use russh_sftp::client::error::Error;
    let kind = match &error {
        Error::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => io::ErrorKind::NotFound,
            StatusCode::PermissionDenied => io::ErrorKind::PermissionDenied,
            StatusCode::OpUnsupported => io::ErrorKind::Unsupported,
            _ => io::ErrorKind::Other,
        },
        Error::Timeout => io::ErrorKind::TimedOut,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, error.to_string())
}

fn meta_of(attrs: &FileAttributes) -> Meta {
    let kind = if attrs.is_symlink() {
        Kind::Symlink
    } else if attrs.is_dir() {
        Kind::Dir
    } else if attrs.is_regular() {
        Kind::File
    } else {
        Kind::Other
    };
    Meta {
        kind,
        perm: attrs.permissions.unwrap_or(0) & 0o7777,
    }
}

fn mode_attrs(mode: u32) -> FileAttributes {
    FileAttributes {
        permissions: Some(mode),
        ..FileAttributes::empty()
    }
}

fn optional<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

impl RemoteFs for Sftp {
    type File = russh_sftp::client::fs::File;

    async fn home(&self) -> io::Result<String> {
        self.0.canonicalize(".").await.map_err(sftp_error)
    }

    async fn stat(&self, path: &str) -> io::Result<Option<Meta>> {
        optional(self.0.metadata(path).await.map_err(sftp_error)).map(|m| m.map(|m| meta_of(&m)))
    }

    async fn lstat(&self, path: &str) -> io::Result<Option<Meta>> {
        optional(self.0.symlink_metadata(path).await.map_err(sftp_error))
            .map(|m| m.map(|m| meta_of(&m)))
    }

    async fn mkdir(&self, path: &str) -> io::Result<()> {
        self.0.create_dir(path).await.map_err(sftp_error)
    }

    async fn chmod(&self, path: &str, mode: u32) -> io::Result<()> {
        self.0
            .set_metadata(path, mode_attrs(mode))
            .await
            .map_err(sftp_error)
    }

    async fn list(&self, dir: &str) -> io::Result<Vec<(String, Kind)>> {
        let entries = self.0.read_dir(dir).await.map_err(sftp_error)?;
        Ok(entries
            .map(|entry| (entry.file_name(), meta_of(&entry.metadata()).kind))
            .collect())
    }

    async fn create_new(&self, path: &str) -> io::Result<Self::File> {
        // 请求 600；不理会的服务器之后由 file_chmod 改
        self.0
            .open_with_flags_and_attributes(
                path,
                OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                mode_attrs(0o600),
            )
            .await
            .map_err(sftp_error)
    }

    async fn file_chmod(&self, file: &mut Self::File, mode: u32) -> io::Result<()> {
        file.set_metadata(mode_attrs(mode))
            .await
            .map_err(sftp_error)
    }

    async fn file_write(&self, file: &mut Self::File, data: &[u8]) -> io::Result<()> {
        file.write_all(data).await
    }

    async fn file_close(&self, file: Self::File) -> io::Result<()> {
        file.close().await
    }

    async fn remove_file(&self, path: &str) -> io::Result<()> {
        self.0.remove_file(path).await.map_err(sftp_error)
    }

    async fn remove_dir(&self, path: &str) -> io::Result<()> {
        self.0.remove_dir(path).await.map_err(sftp_error)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use chrono::NaiveDate;
    use regex::Regex;

    use super::*;
    use crate::testkit::block_on;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn at(y: i32, m: u32, d: u32, h: u32, min: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    #[test]
    fn checks_size_and_format() {
        assert_eq!(check(PNG).unwrap(), ImageFormat::Png);
        assert_eq!(check(b"\xff\xd8\xff\xe0").unwrap(), ImageFormat::Jpeg);
        assert_eq!(check(b"GIF89a....").unwrap(), ImageFormat::Gif);
        assert_eq!(check(b"RIFF\0\0\0\0WEBPVP8 ").unwrap(), ImageFormat::Webp);
        let unsupported = |data: &[u8]| match check(data) {
            Err(CoreError::Invalid(invalid)) => invalid.reason,
            other => panic!("{other:?}"),
        };
        assert_eq!(unsupported(b"BM\0\0"), "unsupported_format");
        assert_eq!(unsupported(b""), "unsupported_format");
        assert_eq!(unsupported(b"RIFF\0\0\0\0WAVE"), "unsupported_format");
        let mut big = PNG.to_vec();
        big.resize(MAX_IMAGE_BYTES + 1, 0);
        assert_eq!(unsupported(&big), "too_large");
        big.truncate(MAX_IMAGE_BYTES);
        assert!(check(&big).is_ok());
    }

    // 存到目录中并返回路径；只保留最近的 20 张、7 天以内的，其他文件不动
    #[test]
    fn local_saves_and_prunes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pastes");
        let first = save_local(&dir, PNG).unwrap();
        let name = first.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            Regex::new(r"^termlet-paste-\d{8}-\d{6}-[0-9a-f]{8}\.png$")
                .unwrap()
                .is_match(&name),
            "{name}"
        );
        assert_eq!(fs::read(&first).unwrap(), PNG);
        assert!(platform::is_private_dir(&dir));

        // 8 天前的截图被清理；其他文件不动
        let old = dir.join("termlet-paste-20000101-000000-aaaaaaaa.png");
        fs::write(&old, PNG).unwrap();
        let eight_days_ago = SystemTime::now() - Duration::from_secs(8 * 24 * 3600);
        fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(eight_days_ago)
            .unwrap();
        fs::write(dir.join("other.txt"), b"x").unwrap();
        let mut last = first;
        for _ in 0..22 {
            last = save_local(&dir, PNG).unwrap();
        }
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"other.txt".to_string()));
        assert!(!old.exists());
        assert!(last.exists(), "刚写入的不应被清理");
        assert_eq!(
            names.iter().filter(|n| n.starts_with(PREFIX)).count(),
            KEEP_FILES
        );
    }

    #[test]
    fn local_rejects_bad_image_before_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("pastes");
        assert!(save_local(&dir, b"not an image").is_err());
        assert!(!dir.exists());
    }

    #[test]
    fn path_text_is_quoted_when_needed() {
        assert_eq!(
            remote_path_text(
                "/home/u/.cache/termlet/paste/termlet-paste-20261014-210500-0a1b2c3d.png"
            ),
            "/home/u/.cache/termlet/paste/termlet-paste-20261014-210500-0a1b2c3d.png"
        );
        assert_eq!(
            remote_path_text("/home/my user/a.png"),
            "'/home/my user/a.png'"
        );
        assert_eq!(
            remote_path_text("/home/o'neil/a.png"),
            r#"'/home/o'"'"'neil/a.png'"#
        );
        assert_eq!(remote_path_text("/home/张三/a.png"), "'/home/张三/a.png'");
        assert_eq!(remote_path_text("/a/$HOME"), "'/a/$HOME'");

        let windows = Path::new(r"C:\Users\ruan\AppData\Local\Temp\termlet-pastes\a.png");
        let spaced = Path::new(r"C:\Users\Ruan Feng\Temp\a.png");
        assert_eq!(
            local_path_text(windows, PathStyle::Windows),
            r"C:\Users\ruan\AppData\Local\Temp\termlet-pastes\a.png"
        );
        assert_eq!(
            local_path_text(spaced, PathStyle::Windows),
            r#""C:\Users\Ruan Feng\Temp\a.png""#
        );
        assert_eq!(
            local_path_text(windows, PathStyle::Wsl),
            "/mnt/c/Users/ruan/AppData/Local/Temp/termlet-pastes/a.png"
        );
        assert_eq!(
            local_path_text(spaced, PathStyle::Wsl),
            "'/mnt/c/Users/Ruan Feng/Temp/a.png'"
        );
        if cfg!(windows) {
            assert_eq!(
                local_path_text(windows, PathStyle::Posix),
                "C:/Users/ruan/AppData/Local/Temp/termlet-pastes/a.png"
            );
        }
        assert_eq!(
            local_path_text(Path::new("/tmp/termlet-pastes/a.png"), PathStyle::Posix),
            "/tmp/termlet-pastes/a.png"
        );
    }

    #[test]
    fn expiry_uses_time_in_name() {
        let now = at(2026, 10, 15, 9, 0);
        assert!(!expired("termlet-paste-20261014-090000-11111111.png", now));
        assert!(expired("termlet-paste-20261005-090000-22222222.png", now));
        assert!(!expired("termlet-paste-20261008-090100-22222222.png", now));
        assert!(!expired("termlet-paste-garbage.png", now));
        assert!(is_paste_name("termlet-paste-20261005-090000-22222222.webp"));
        assert!(!is_paste_name("termlet-paste-20261005-090000-22222222.txt"));
        assert!(!is_paste_name("termlet-paste-png"));
        assert!(!is_paste_name("other.png"));
    }

    /// 内存中的 SFTP 文件系统：记录每个路径的权限与内容，可以模拟不允许修改权限、用户目录不可写。
    struct MemFs(Mutex<MemState>);

    struct MemState {
        home: String,
        dirs: HashMap<String, u32>,
        files: HashMap<String, MemFile>,
        symlinks: HashSet<String>,
        /// 这些路径（前缀）不允许修改权限
        no_chmod: Vec<String>,
        /// 这些路径（前缀）下不能新建
        read_only: Vec<String>,
    }

    #[derive(Clone)]
    struct MemFile {
        mode: u32,
        data: Vec<u8>,
    }

    fn parent(path: &str) -> &str {
        match path.rsplit_once('/') {
            Some(("", _)) => "/",
            Some((parent, _)) => parent,
            None => "",
        }
    }

    fn has_prefix(list: &[String], path: &str) -> bool {
        list.iter().any(|p| path.starts_with(p.as_str()))
    }

    fn unsupported() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "SSH_FX_OP_UNSUPPORTED")
    }

    impl MemFs {
        fn new(home: &str) -> Self {
            let dirs = HashMap::from([
                ("/".to_string(), 0o755),
                ("/tmp".to_string(), 0o1777),
                ("/home".to_string(), 0o755),
                (home.to_string(), 0o755),
            ]);
            Self(Mutex::new(MemState {
                home: home.into(),
                dirs,
                files: HashMap::new(),
                symlinks: HashSet::new(),
                no_chmod: Vec::new(),
                read_only: Vec::new(),
            }))
        }

        fn state(&self) -> std::sync::MutexGuard<'_, MemState> {
            self.0.lock().unwrap()
        }

        fn put(&self, path: &str) {
            self.state().files.insert(
                path.into(),
                MemFile {
                    mode: 0o600,
                    data: b"x".to_vec(),
                },
            );
        }
    }

    impl RemoteFs for MemFs {
        type File = String;

        async fn home(&self) -> io::Result<String> {
            Ok(self.state().home.clone())
        }

        async fn stat(&self, path: &str) -> io::Result<Option<Meta>> {
            let s = self.state();
            if s.symlinks.contains(path) {
                // 指向一个共享的目录
                return Ok(Some(Meta {
                    kind: Kind::Dir,
                    perm: 0o777,
                }));
            }
            self.lstat_locked(&s, path)
        }

        async fn lstat(&self, path: &str) -> io::Result<Option<Meta>> {
            let s = self.state();
            if s.symlinks.contains(path) {
                return Ok(Some(Meta {
                    kind: Kind::Symlink,
                    perm: 0o777,
                }));
            }
            self.lstat_locked(&s, path)
        }

        async fn mkdir(&self, path: &str) -> io::Result<()> {
            let mut s = self.state();
            if s.dirs.contains_key(path) || s.symlinks.contains(path) || s.files.contains_key(path)
            {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            if !s.dirs.contains_key(parent(path)) || has_prefix(&s.read_only, path) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            // 新建时按 umask，不是 700
            s.dirs.insert(path.into(), 0o755);
            Ok(())
        }

        async fn chmod(&self, path: &str, mode: u32) -> io::Result<()> {
            let mut s = self.state();
            if has_prefix(&s.no_chmod, path) {
                return Err(unsupported());
            }
            if s.symlinks.contains(path) {
                // 修改的是链接指向的目录
                return Ok(());
            }
            match s.dirs.get_mut(path) {
                Some(perm) => {
                    *perm = mode;
                    Ok(())
                }
                None => Err(io::ErrorKind::NotFound.into()),
            }
        }

        async fn list(&self, dir: &str) -> io::Result<Vec<(String, Kind)>> {
            let s = self.state();
            Ok(s.files
                .keys()
                .filter(|p| parent(p) == dir)
                .map(|p| (p.rsplit('/').next().unwrap().to_string(), Kind::File))
                .collect())
        }

        async fn create_new(&self, path: &str) -> io::Result<String> {
            let mut s = self.state();
            if s.files.contains_key(path) || s.symlinks.contains(path) {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            if !s.dirs.contains_key(parent(path)) || has_prefix(&s.read_only, path) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            // 不理会请求的权限
            s.files.insert(
                path.into(),
                MemFile {
                    mode: 0o644,
                    data: Vec::new(),
                },
            );
            Ok(path.into())
        }

        async fn file_chmod(&self, file: &mut String, mode: u32) -> io::Result<()> {
            let mut s = self.state();
            if has_prefix(&s.no_chmod, file) {
                return Err(unsupported());
            }
            s.files.get_mut(file.as_str()).unwrap().mode = mode;
            Ok(())
        }

        async fn file_write(&self, file: &mut String, data: &[u8]) -> io::Result<()> {
            let mut s = self.state();
            s.files
                .get_mut(file.as_str())
                .unwrap()
                .data
                .extend_from_slice(data);
            Ok(())
        }

        async fn file_close(&self, _file: String) -> io::Result<()> {
            Ok(())
        }

        async fn remove_file(&self, path: &str) -> io::Result<()> {
            match self.state().files.remove(path) {
                Some(_) => Ok(()),
                None => Err(io::ErrorKind::NotFound.into()),
            }
        }

        async fn remove_dir(&self, path: &str) -> io::Result<()> {
            match self.state().dirs.remove(path) {
                Some(_) => Ok(()),
                None => Err(io::ErrorKind::NotFound.into()),
            }
        }
    }

    impl MemFs {
        fn lstat_locked(&self, s: &MemState, path: &str) -> io::Result<Option<Meta>> {
            if let Some(perm) = s.dirs.get(path) {
                return Ok(Some(Meta {
                    kind: Kind::Dir,
                    perm: *perm,
                }));
            }
            Ok(s.files.get(path).map(|f| Meta {
                kind: Kind::File,
                perm: f.mode,
            }))
        }
    }

    fn write(fs: &MemFs, data: &[u8], now: NaiveDateTime) -> Result<String> {
        block_on(write_paste(fs, data, ImageFormat::Png, now, &random_hex()))
    }

    const PASTE_DIR: &str = "/home/u/.cache/termlet/paste";

    // 截图写在只有自己可访问的目录中：首选 ~/.cache/termlet/paste（700），文件为 600。
    #[test]
    fn remote_writes_into_private_home_dir() {
        let fs = MemFs::new("/home/u");
        let path = write(&fs, PNG, at(2026, 10, 14, 21, 5)).unwrap();
        assert_eq!(parent(&path), PASTE_DIR);
        assert!(
            path.rsplit('/')
                .next()
                .unwrap()
                .starts_with("termlet-paste-20261014-210500-"),
            "{path}"
        );
        let s = fs.state();
        assert_eq!(s.dirs[PASTE_DIR], 0o700);
        assert_eq!(s.files[&path].mode, 0o600);
        assert_eq!(s.files[&path].data, PNG);
    }

    // 用户目录不可写时在 /tmp 下新建私有目录；不沿用别人事先建好的目录或符号链接。
    #[test]
    fn remote_falls_back_to_new_tmp_dir() {
        let fs = MemFs::new("/home/u");
        fs.state().read_only = vec!["/home/u".into()];
        let path = write(&fs, PNG, now_local()).unwrap();
        let dir = parent(&path).to_string();
        assert!(dir.starts_with("/tmp/termlet-paste-"), "{path}");
        assert_eq!(fs.state().dirs[&dir], 0o700);
        assert_eq!(fs.state().files[&path].mode, 0o600);

        // ~/.cache/termlet/paste 是符号链接（指向共享目录）：不使用
        let fs = MemFs::new("/home/u");
        block_on(mkdir_all(&fs, "/home/u/.cache/termlet")).unwrap();
        fs.state().symlinks.insert(PASTE_DIR.into());
        let path = write(&fs, PNG, now_local()).unwrap();
        assert!(path.starts_with("/tmp/termlet-paste-"), "{path}");

        // 没有用户目录（SFTP 的当前目录是 /）
        let fs = MemFs::new("/");
        let path = write(&fs, PNG, now_local()).unwrap();
        assert!(path.starts_with("/tmp/termlet-paste-"), "{path}");
    }

    // 服务器不允许修改权限：不留下其他用户可读的截图，报错。
    #[test]
    fn remote_refuses_when_permissions_cannot_be_set() {
        // 目录与文件都不能修改权限
        let fs = MemFs::new("/home/u");
        fs.state().no_chmod = vec!["/".into()];
        let error = write(&fs, PNG, now_local()).unwrap_err();
        assert!(
            matches!(&error, CoreError::Io(e) if e.kind() == io::ErrorKind::PermissionDenied),
            "{error:?}"
        );
        let s = fs.state();
        assert!(s.files.is_empty(), "不应留下文件");
        assert!(
            !s.dirs.keys().any(|d| d.starts_with("/tmp/termlet-paste-")),
            "不应留下 /tmp 中新建的目录"
        );
        drop(s);

        // 目录可以、文件不能修改权限：删除已创建的文件
        let fs = MemFs::new("/home/u");
        fs.state().no_chmod = vec![format!("{PASTE_DIR}/")];
        assert!(write(&fs, PNG, now_local()).is_err());
        assert!(fs.state().files.is_empty(), "文件不能改为 600 时应删除");
    }

    // 只保留最近 20 张、7 天以内的截图，其他文件不动。
    #[test]
    fn remote_prunes_old_pastes() {
        let fs = MemFs::new("/home/u");
        block_on(mkdir_all(&fs, PASTE_DIR)).unwrap();
        let now = at(2026, 10, 14, 12, 0);
        for i in 0..25i64 {
            let t = now - chrono::Duration::hours(i);
            fs.put(&format!(
                "{PASTE_DIR}/termlet-paste-{}-{i:08x}.png",
                t.format(NAME_TIME)
            ));
        }
        let old = format!("{PASTE_DIR}/termlet-paste-20261001-000000-aaaaaaaa.png");
        fs.put(&old);
        let other = format!("{PASTE_DIR}/notes.txt");
        fs.put(&other);

        let path = write(&fs, PNG, now).unwrap();
        let s = fs.state();
        assert!(s.files.contains_key(&path));
        assert!(!s.files.contains_key(&old));
        assert!(s.files.contains_key(&other));
        assert_eq!(
            s.files.len(),
            KEEP_FILES + 1,
            "应保留最近 20 张（含这次的）与其他文件"
        );
    }

    // 服务器的时钟落后本机 7 天以上：刚写入的截图不能被当作过期删除；是否过期按文件名中的本机时间判断。
    #[test]
    fn remote_expiry_ignores_server_clock() {
        let fs = MemFs::new("/home/u");
        block_on(mkdir_all(&fs, PASTE_DIR)).unwrap();
        let now = at(2026, 10, 15, 9, 0);
        // 昨天粘贴的：保留
        let recent = format!("{PASTE_DIR}/termlet-paste-20261014-090000-11111111.png");
        fs.put(&recent);
        // 10 天前粘贴的：删除
        let old = format!("{PASTE_DIR}/termlet-paste-20261005-090000-22222222.png");
        fs.put(&old);

        let path = write(&fs, PNG, now).unwrap();
        let s = fs.state();
        assert!(s.files.contains_key(&path), "刚写入的截图不应被删除");
        assert!(s.files.contains_key(&recent));
        assert!(!s.files.contains_key(&old));
    }

    #[test]
    fn mkdir_all_rejects_file_in_the_way() {
        let fs = MemFs::new("/home/u");
        fs.put("/home/u/.cache");
        assert!(block_on(mkdir_all(&fs, PASTE_DIR)).is_err());
        let fs = MemFs::new("/home/u");
        block_on(mkdir_all(&fs, PASTE_DIR)).unwrap();
        // 已存在时什么也不做
        block_on(mkdir_all(&fs, PASTE_DIR)).unwrap();
        assert!(fs.state().dirs.contains_key("/home/u/.cache/termlet"));
    }

    /// 界面在核心的运行时上执行上传：返回的 future 必须能跨线程（编译时检查）。
    fn _upload_future_is_send(connection: &Connection) {
        fn assert_send<T: Send>(_: &T) {}
        assert_send(&upload(connection, PNG));
    }

    /// 对真实 sshd 的集成测试：设置 `TERMLET_TEST_SSH=127.0.0.1:2222`（testing/sshd 的测试容器）后运行，否则跳过。
    #[test]
    fn upload_to_real_server() {
        use crate::ssh::exec;
        use crate::testkit::{connect_test, test_server};

        let Some((host, port)) = test_server() else {
            return;
        };
        block_on(async {
            let connection = connect_test(&host, port).await;

            let path = upload(&connection, PNG).await.unwrap();
            assert!(
                path.contains("/.cache/termlet/paste/termlet-paste-"),
                "{path}"
            );
            assert!(path.ends_with(".png"));
            let dir = parent(&path).to_string();
            let quoted = remote_path_text(&path);
            let command = format!(
                "stat -c %a {} {quoted}; wc -c < {quoted}; rm -f {quoted}",
                remote_path_text(&dir)
            );
            let result = exec(&connection, &command, 4096, Duration::from_secs(10))
                .await
                .unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout);
            let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
            let size = PNG.len().to_string();
            assert_eq!(lines, vec!["700", "600", size.as_str()], "{stdout}");
        });
    }
}
