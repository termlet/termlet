//! 远程文本文件的在线编辑：整文件读取与保存（编辑器 Tab 使用）。
//!
//! 保存时先写同目录下的临时文件再改名替换原文件：保存中途断开不会留下写了一半的文件。保留原文件的权限与属主，
//! 新文件的权限为 0644；路径是符号链接时写入它指向的文件，链接本身保持不变。目录不可写（无法创建临时文件）
//! 或无法保持原文件的属主（例如文件属于其他用户、只是组内可写）时，改为直接覆盖写入原文件。
//! 保存前检查远程文件在打开后有没有被别人改过（修改时间或大小变了）、被删除。
//!
//! SCP 模式（服务器没有 SFTP）：经 SCP 读写，保存同样先写同目录的临时文件再 `mv` 替换；临时文件的属主、属组与原文件
//! 不同（SCP 改不了属主）或目录不可写时直接覆盖写入原文件。
//!
//! 文件内容的编码是 [`TextEncoding`]：终端、主机设置用的 [`Encoding`](crate::model::Encoding)（UTF-8、GB18030、
//! Big5）之外，还有 UTF-16LE、UTF-16BE（Windows PowerShell 5.1 重定向生成的文本、Windows 的日志与导出的 CSV、
//! .reg 文件）。不指定编码时自动识别：合法的 UTF-8 → 带 BOM 的 UTF-16 → 没有 BOM 但 NUL 字节明显集中在奇数位（LE）
//! 或偶数位（BE）、能完整解码的 UTF-16；都不是时为 [`SftpError::NotText`]。BOM 与 UTF-8 的一样留在内容开头
//! （U+FEFF），保存时随内容写回，所以原来有没有 BOM 保存后不变。

use std::sync::atomic::AtomicU64;

use russh_sftp::protocol::{FileAttributes, OpenFlags};

use super::SftpSession;
use super::backend::Backend;
use super::cancel::Cancel;
use super::client::{
    PipeError, SftpClient, is_regular, is_symlink, mtime_of, perm_attrs, perm_bits, size_of,
};
use super::entry::mode_string;
use super::error::{self, Rejection, SftpError, Side, is_permission};
use super::names::{random_hex, remote_name_fits, tagged_name};
use super::path;
use super::scp::{ScpRemote, scp_error};
use super::transfer::upload::rename_replacing;

/// 在线编辑的文件大小上限（字节）。
pub const MAX_EDIT_SIZE: u64 = 5 << 20;

/// 保存时新建文件的权限。
const NEW_FILE_MODE: u32 = 0o644;

/// 文本文件内容的编码（只用于在线编辑，不进设置；终端与文件名的编码见
/// [`Encoding`](crate::model::Encoding)）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TextEncoding {
    #[default]
    Utf8,
    Gb18030,
    Big5,
    Utf16Le,
    Utf16Be,
}

impl TextEncoding {
    /// 全部编码，按“按其他编码重新打开”中列出的顺序。
    pub const ALL: [TextEncoding; 5] = [
        TextEncoding::Utf8,
        TextEncoding::Gb18030,
        TextEncoding::Big5,
        TextEncoding::Utf16Le,
        TextEncoding::Utf16Be,
    ];

    /// 显示用的名称（专有名词，不翻译）。
    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Gb18030 => "GBK / GB18030",
            TextEncoding::Big5 => "Big5",
            TextEncoding::Utf16Le => "UTF-16LE",
            TextEncoding::Utf16Be => "UTF-16BE",
        }
    }
}

/// 读取到的远程文本文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextFile {
    /// 解析后的绝对路径（不展开符号链接）
    pub path: String,
    /// 解码后的内容；文件带 BOM 时以 U+FEFF 开头（保存时随内容写回）
    pub content: String,
    pub encoding: TextEncoding,
    /// 字节数
    pub size: u64,
    /// 修改时间（Unix 秒）：保存时作为 `expected_modified` 提交，用于发现冲突
    pub modified: i64,
    /// `ls -l` 的写法，例如 `-rw-r--r--`
    pub mode: String,
    pub perm: u32,
}

/// 保存文本文件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteTextRequest {
    pub path: String,
    pub content: String,
    /// 保存用的编码（打开时的编码）；为空时是 UTF-8
    pub encoding: Option<TextEncoding>,
    /// 打开（或上次保存）时远程文件的修改时间与大小：远程文件与之不同（被别人修改或已删除）时返回
    /// [`SftpError::Changed`]；都不给时不检查
    pub expected_modified: Option<i64>,
    pub expected_size: Option<u64>,
    /// 不检查冲突，直接覆盖
    pub force: bool,
}

/// 保存后的远程文件信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteResult {
    pub path: String,
    pub size: u64,
    pub modified: i64,
    pub mode: String,
    pub perm: u32,
}

/// 内容能否作为 UTF-8 文本编辑：不含 NUL 字节且是合法的 UTF-8（带 BOM 的也算）。
pub fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn charset(encoding: TextEncoding) -> &'static encoding_rs::Encoding {
    match encoding {
        TextEncoding::Utf8 => encoding_rs::UTF_8,
        TextEncoding::Gb18030 => encoding_rs::GB18030,
        TextEncoding::Big5 => encoding_rs::BIG5,
        TextEncoding::Utf16Le => encoding_rs::UTF_16LE,
        TextEncoding::Utf16Be => encoding_rs::UTF_16BE,
    }
}

/// 按编码解码；含 NUL 字符或不是合法的编码时为 `None`。BOM 不去掉（以 U+FEFF 留在开头）；按 UTF-16 解码时
/// 奇数长度、孤立的代理项不合法，开头是 U+FFFE（字节序反了）也为 `None`。
fn decode(bytes: &[u8], encoding: TextEncoding) -> Option<String> {
    match encoding {
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            let text =
                charset(encoding).decode_without_bom_handling_and_without_replacement(bytes)?;
            (!text.contains('\0') && !text.starts_with('\u{fffe}')).then(|| text.into_owned())
        }
        _ if bytes.contains(&0) => None,
        TextEncoding::Utf8 => String::from_utf8(bytes.to_vec()).ok(),
        TextEncoding::Gb18030 | TextEncoding::Big5 => charset(encoding)
            .decode_without_bom_handling_and_without_replacement(bytes)
            .map(|text| text.into_owned()),
    }
}

/// 自动识别编码并解码（见模块说明）；不是文本时为 `None`。
fn detect(bytes: &[u8]) -> Option<(String, TextEncoding)> {
    if let Some(text) = decode(bytes, TextEncoding::Utf8) {
        return Some((text, TextEncoding::Utf8));
    }
    let bom = match bytes {
        [0xff, 0xfe, ..] => Some(TextEncoding::Utf16Le),
        [0xfe, 0xff, ..] => Some(TextEncoding::Utf16Be),
        _ => None,
    };
    let encoding = bom.or_else(|| utf16_without_bom(bytes))?;
    let text = decode(bytes, encoding)?;
    // 有 BOM 的照 BOM 打开；没有 BOM 的再看解出来的像不像文本
    (bom.is_some() || looks_like_text(&text)).then_some((text, encoding))
}

/// 没有 BOM 时能否明确判断为 UTF-16：偶数长度，至少一半的码元一边的字节是 NUL（以 ASCII 为主的文本：LE 是
/// `[字符, 0]`，BE 是 `[0, 字符]`），另一边的 NUL 不超过一成（`U+4E00` 这样低位是 0 的汉字）。以汉字为主的文本
/// NUL 很少，判断不了，按不是文本处理（界面上可以手动选 UTF-16 打开）。
fn utf16_without_bom(bytes: &[u8]) -> Option<TextEncoding> {
    if bytes.is_empty() || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let units = pairs.len();
    let (mut first, mut second) = (0, 0);
    for [a, b] in pairs {
        first += usize::from(*a == 0);
        second += usize::from(*b == 0);
    }
    let clear = |many: usize, few: usize| many * 2 >= units && few * 10 <= units;
    if clear(second, first) {
        Some(TextEncoding::Utf16Le)
    } else if clear(first, second) {
        Some(TextEncoding::Utf16Be)
    } else {
        None
    }
}

/// 解出来的像文本：控制字符只有常见的空白（制表、换行、回车、换页）与 ESC，没有 U+FFFE、U+FFFF。
fn looks_like_text(text: &str) -> bool {
    !text.chars().any(|c| {
        (c.is_control() && !matches!(c, '\t' | '\n' | '\r' | '\x0c' | '\x1b'))
            || matches!(c, '\u{fffe}' | '\u{ffff}')
    })
}

fn encode(text: &str, encoding: TextEncoding) -> Result<Vec<u8>, SftpError> {
    match encoding {
        TextEncoding::Utf8 => Ok(text.as_bytes().to_vec()),
        // encoding_rs 不能编码成 UTF-16（会输出 UTF-8），自己写：encode_utf16 把 BMP 之外的字符拆成代理对
        TextEncoding::Utf16Le => Ok(text.encode_utf16().flat_map(u16::to_le_bytes).collect()),
        TextEncoding::Utf16Be => Ok(text.encode_utf16().flat_map(u16::to_be_bytes).collect()),
        TextEncoding::Gb18030 | TextEncoding::Big5 => {
            let (bytes, _, unmappable) = charset(encoding).encode(text);
            if unmappable {
                return Err(Rejection::Unencodable.into());
            }
            Ok(bytes.into_owned())
        }
    }
}

fn too_large(path: &str) -> SftpError {
    SftpError::TooLarge {
        path: path.to_string(),
        limit: MAX_EDIT_SIZE,
    }
}

impl SftpSession {
    /// 读取远程文本文件。`encoding` 为空时自动识别（UTF-8、UTF-16，见模块说明），识别不了时返回
    /// [`SftpError::NotText`]（界面可以让用户选别的编码打开）；指定了编码时按它解码，解不了同样返回
    /// [`SftpError::NotText`]。超过 [`MAX_EDIT_SIZE`] 返回 [`SftpError::TooLarge`]。
    pub async fn read_text(
        &self,
        p: &str,
        encoding: Option<TextEncoding>,
    ) -> Result<TextFile, SftpError> {
        let client = self.backend();
        if self.closing.is_canceled() {
            return Err(SftpError::SessionClosed);
        }
        let p = self.resolve(p);
        let attrs = client.stat(&p).await.map_err(|e| error::remote(&p, &e))?;
        if !is_regular(&attrs) {
            return Err(Rejection::NotRegularFile.into());
        }
        if size_of(&attrs) > MAX_EDIT_SIZE {
            return Err(too_large(&p));
        }
        let bytes = match &client {
            Backend::Sftp(sftp) => read_all(sftp, &p, MAX_EDIT_SIZE).await?,
            Backend::Scp(scp) => {
                let mut buf = Vec::new();
                scp.download(
                    &p,
                    &mut buf,
                    &Cancel::new(),
                    &AtomicU64::new(0),
                    Some(MAX_EDIT_SIZE),
                )
                .await
                .map_err(|f| scp_error(&p, f))?;
                buf
            }
        };
        let decoded = match encoding {
            Some(encoding) => decode(&bytes, encoding).map(|text| (text, encoding)),
            None => detect(&bytes),
        };
        tracing::debug!(
            session = %self.id,
            path = %p,
            bytes = bytes.len(),
            requested = ?encoding,
            encoding = ?decoded.as_ref().map(|(_, e)| *e),
            "读取远程文本文件"
        );
        let (content, encoding) = decoded.ok_or_else(|| SftpError::NotText { path: p.clone() })?;
        Ok(TextFile {
            path: p,
            content,
            encoding,
            size: bytes.len() as u64,
            modified: mtime_of(&attrs),
            mode: mode_string(attrs.permissions.unwrap_or(0o100644)),
            perm: perm_bits(&attrs),
        })
    }

    /// 保存文本文件（见模块说明）。
    pub async fn write_text(&self, request: &WriteTextRequest) -> Result<WriteResult, SftpError> {
        let client = self.backend();
        if self.closing.is_canceled() {
            return Err(SftpError::SessionClosed);
        }
        let p = self.resolve(&request.path);
        let data = encode(&request.content, request.encoding.unwrap_or_default())?;
        if data.len() as u64 > MAX_EDIT_SIZE {
            return Err(too_large(&p));
        }
        let mut target = p.clone();
        let orig = match client.stat(&p).await {
            Ok(attrs) => {
                if !is_regular(&attrs) {
                    return Err(Rejection::NotRegularFile.into());
                }
                let changed = request
                    .expected_modified
                    .is_some_and(|m| m != mtime_of(&attrs))
                    || request.expected_size.is_some_and(|s| s != size_of(&attrs));
                if !request.force && changed {
                    tracing::info!(session = %self.id, path = %p, "远程文件在打开后被修改过，保存前请用户确认");
                    return Err(SftpError::Changed {
                        path: p,
                        deleted: false,
                    });
                }
                if client.lstat(&p).await.is_ok_and(|l| is_symlink(&l)) {
                    target = match &client {
                        Backend::Sftp(sftp) => sftp.realpath(&p).await,
                        Backend::Scp(scp) => scp.resolve_link(&p).await,
                    }
                    .map_err(|e| error::remote(&p, &e))?;
                }
                Some(attrs)
            }
            Err(e) if error::is_not_found(&e) => {
                if !request.force
                    && (request.expected_modified.is_some() || request.expected_size.is_some())
                {
                    return Err(SftpError::Changed {
                        path: p,
                        deleted: true,
                    });
                }
                None
            }
            Err(e) => return Err(error::remote(&p, &e)),
        };
        tracing::debug!(
            session = %self.id,
            path = %p,
            target = %target,
            bytes = data.len(),
            force = request.force,
            existed = orig.is_some(),
            "保存远程文本文件"
        );
        match &client {
            Backend::Sftp(sftp) => replace_file(sftp, &target, &data, orig.as_ref()).await,
            Backend::Scp(scp) => replace_file_scp(scp, &target, &data, orig.as_ref()).await,
        }
        .inspect_err(
            |e| tracing::warn!(session = %self.id, path = %p, "保存远程文本文件失败：{e}"),
        )?;
        let attrs = client
            .stat(&target)
            .await
            .map_err(|e| error::remote(&target, &e))?;
        Ok(WriteResult {
            path: p,
            size: size_of(&attrs),
            modified: mtime_of(&attrs),
            mode: mode_string(attrs.permissions.unwrap_or(0o100644)),
            perm: perm_bits(&attrs),
        })
    }
}

/// 读出整个文件（并发读取，高延迟链路上比逐块读快得多）；读的过程中文件变大到超过上限时报错，不会无限占用内存。
async fn read_all(client: &SftpClient, p: &str, limit: u64) -> Result<Vec<u8>, SftpError> {
    let handle = client
        .open(p, OpenFlags::READ, FileAttributes::empty())
        .await
        .map_err(|e| error::remote(p, &e))?;
    let mut buf = Vec::new();
    let progress = AtomicU64::new(0);
    let result = client
        .read_into(&handle, 0, &mut buf, &Cancel::new(), &progress, Some(limit))
        .await;
    let _ = client.close_handle(&handle).await;
    match result {
        Ok(_) => Ok(buf),
        Err(f) => Err(match f.error {
            PipeError::TooLarge => too_large(p),
            PipeError::Remote(e) => error::remote(p, &e),
            PipeError::Local(e) => SftpError::failed(Side::Local, p, e.to_string()),
            PipeError::Canceled => SftpError::Canceled,
        }),
    }
}

/// 写入远程文件的全部内容。
async fn write_all(
    client: &SftpClient,
    handle: &str,
    path: &str,
    data: &[u8],
) -> Result<(), SftpError> {
    let mut reader = data;
    let progress = AtomicU64::new(0);
    match client
        .write_from(handle, 0, &mut reader, &Cancel::new(), &progress)
        .await
    {
        Ok(_) => Ok(()),
        Err(f) => Err(match f.error {
            PipeError::Remote(e) => error::remote(path, &e),
            PipeError::Local(e) => SftpError::failed(Side::Local, path, e.to_string()),
            PipeError::Canceled => SftpError::Canceled,
            PipeError::TooLarge => too_large(path),
        }),
    }
}

/// 经临时文件写入 `target`；`orig` 为空表示新建。
async fn replace_file(
    client: &SftpClient,
    target: &str,
    data: &[u8],
    orig: Option<&FileAttributes>,
) -> Result<(), SftpError> {
    let tmp = temp_path(target);
    let handle = match client
        .open(
            &tmp,
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::EXCLUDE,
            FileAttributes::empty(),
        )
        .await
    {
        Ok(handle) => handle,
        Err(e) if orig.is_some() && is_permission(&e) => {
            return write_in_place(client, target, data).await;
        }
        Err(e) => return Err(error::remote(target, &e)),
    };
    let discard = async |error: SftpError| {
        let _ = client.close_handle(&handle).await;
        let _ = client.remove(&tmp).await;
        error
    };

    // 先设置权限与属主再写入内容：权限较严的文件（例如 0600）内容不会经临时文件暴露给其他用户
    let mode = orig.map(perm_bits).unwrap_or(NEW_FILE_MODE);
    if let Err(e) = client.fsetstat(&handle, perm_attrs(mode)).await {
        return Err(discard(error::remote(target, &e)).await);
    }
    if let Some(orig) = orig {
        match keep_owner(client, &handle, orig).await {
            Ok(true) => {}
            Ok(false) => {
                let _ = client.close_handle(&handle).await;
                let _ = client.remove(&tmp).await;
                return write_in_place(client, target, data).await;
            }
            Err(e) => return Err(discard(error::remote(target, &e)).await),
        }
    }
    if let Err(e) = write_all(client, &handle, target, data).await {
        return Err(discard(e).await);
    }
    if let Err(e) = client.close_handle(&handle).await {
        let _ = client.remove(&tmp).await;
        return Err(error::remote(target, &e));
    }

    if let Err(e) = client.posix_rename(&tmp, target).await {
        if !client.rename_unsupported(&e) {
            let _ = client.remove(&tmp).await;
            return Err(error::remote(target, &e));
        }
        // 服务器不支持 posix-rename：经备份替换原文件，不先删除原文件。失败时原文件已恢复（或还在）才删除临时文件
        // （编辑器里还有修改，可以再保存）；原文件没能恢复时临时文件里是唯一一份新内容，保留并在错误中说明
        if let Err(e) = rename_replacing(client, &tmp, target, orig.is_some()).await {
            if client.lstat(target).await.is_ok() || orig.is_none() {
                let _ = client.remove(&tmp).await;
                return Err(e);
            }
            return Err(match e {
                SftpError::ReplaceFailed {
                    path,
                    backup,
                    reason,
                    ..
                } => SftpError::ReplaceFailed {
                    path,
                    backup,
                    kept: Some(tmp),
                    reason,
                },
                other => other,
            });
        }
    }
    Ok(())
}

/// 临时文件的路径：与 `target` 同一个目录。原名接近长度上限时用名称的哈希代替名称，免得服务器拒绝过长的临时文件名、
/// 保存总是失败。
fn temp_path(target: &str) -> String {
    path::join(
        &path::parent(target),
        &tagged_name(
            &format!(".{}", path::base_name(target)),
            &format!(".termlet-{}", random_hex(6)),
            ".tmp",
            remote_name_fits,
        ),
    )
}

/// SCP 模式的保存：经同目录的临时文件替换（权限沿用原文件）；目录不可写，或临时文件的属主、属组与原文件不同
/// （例如 root 编辑别人的文件，SCP 不能改属主）时，直接覆盖写入原文件。
async fn replace_file_scp(
    remote: &ScpRemote,
    target: &str,
    data: &[u8],
    orig: Option<&FileAttributes>,
) -> Result<(), SftpError> {
    let mode = orig.map(perm_bits).unwrap_or(NEW_FILE_MODE);
    let tmp = temp_path(target);
    let progress = AtomicU64::new(0);
    let len = data.len() as u64;
    let mut reader = data;
    if let Err(failure) = remote
        .upload(
            &tmp,
            mode,
            None,
            len,
            &mut reader,
            &Cancel::new(),
            &progress,
        )
        .await
    {
        let _ = remote.remove_file(&tmp).await;
        let error = scp_error(target, failure);
        if orig.is_some() && matches!(error, SftpError::PermissionDenied { .. }) {
            return write_in_place_scp(remote, target, data, mode).await;
        }
        return Err(error);
    }
    if let Some(orig) = orig
        && let (Some(uid), Some(gid)) = (orig.uid, orig.gid)
        && remote
            .lstat(&tmp)
            .await
            .is_ok_and(|st| st.uid != Some(uid) || st.gid != Some(gid))
    {
        let _ = remote.remove_file(&tmp).await;
        return write_in_place_scp(remote, target, data, mode).await;
    }
    if let Err(e) = remote.commit_upload(&tmp, target, Some(mode)).await {
        let _ = remote.remove_file(&tmp).await;
        return Err(error::remote(target, &e));
    }
    Ok(())
}

/// SCP 模式下直接覆盖写入原文件（服务器上的 scp 写完后截到新的长度，权限与属主不变）。
async fn write_in_place_scp(
    remote: &ScpRemote,
    target: &str,
    data: &[u8],
    mode: u32,
) -> Result<(), SftpError> {
    let progress = AtomicU64::new(0);
    let mut reader = data;
    remote
        .upload(
            target,
            mode,
            None,
            data.len() as u64,
            &mut reader,
            &Cancel::new(),
            &progress,
        )
        .await
        .map_err(|f| scp_error(target, f))
}

/// 让临时文件的属主、属组与原文件一致。做不到时返回 `false`（调用方改为直接覆盖写入）。
async fn keep_owner(
    client: &SftpClient,
    handle: &str,
    orig: &FileAttributes,
) -> Result<bool, russh_sftp::client::error::Error> {
    let (Some(uid), Some(gid)) = (orig.uid, orig.gid) else {
        return Ok(true);
    };
    let current = client.fstat(handle).await?;
    if current.uid.is_none() || (current.uid == Some(uid) && current.gid == Some(gid)) {
        return Ok(true);
    }
    let owner = FileAttributes {
        uid: Some(uid),
        gid: Some(gid),
        ..FileAttributes::empty()
    };
    // 没有权限修改属主时改为直接覆盖写入，不是错误
    Ok(client.fsetstat(handle, owner).await.is_ok())
}

/// 直接截断并写入原文件（保留 inode、属主与权限，但写入中途断开会留下不完整的文件）。
async fn write_in_place(client: &SftpClient, target: &str, data: &[u8]) -> Result<(), SftpError> {
    let handle = client
        .open(
            target,
            OpenFlags::WRITE | OpenFlags::TRUNCATE,
            FileAttributes::empty(),
        )
        .await
        .map_err(|e| error::remote(target, &e))?;
    if let Err(e) = write_all(client, &handle, target, data).await {
        let _ = client.close_handle(&handle).await;
        return Err(e);
    }
    client
        .close_handle(&handle)
        .await
        .map_err(|e| error::remote(target, &e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_detection() {
        for (input, want) in [
            (&b""[..], true),
            (b"hello\n", true),
            ("中文\r\n".as_bytes(), true),
            (b"\xef\xbb\xbfbom", true),
            (b"a\x00b", false),
            (b"\xff\xfe", false),
            (b"\xe4\xb8", false),
            (b"tab\tesc\x1b", true),
        ] {
            assert_eq!(is_text(input), want, "{input:?}");
        }
        // GB18030 的"中文"
        let gbk = [0xd6, 0xd0, 0xce, 0xc4];
        assert_eq!(decode(&gbk, TextEncoding::Utf8), None);
        assert_eq!(detect(&gbk), None, "不自动识别 GB18030");
        assert_eq!(decode(&gbk, TextEncoding::Gb18030).as_deref(), Some("中文"));
        assert_eq!(encode("中文", TextEncoding::Gb18030).unwrap(), gbk);
        assert!(matches!(
            encode("😀", TextEncoding::Big5),
            Err(SftpError::Rejected(Rejection::Unencodable))
        ));
    }

    fn utf16(text: &str, big_endian: bool, bom: bool) -> Vec<u8> {
        let text = if bom {
            format!("\u{feff}{text}")
        } else {
            text.to_string()
        };
        text.encode_utf16()
            .flat_map(|u| {
                if big_endian {
                    u.to_be_bytes()
                } else {
                    u.to_le_bytes()
                }
            })
            .collect()
    }

    // UTF-8（带不带 BOM）照旧：BOM 以 U+FEFF 留在内容开头，原样写回
    #[test]
    fn utf8_with_bom_is_unchanged() {
        let bytes = b"\xef\xbb\xbfname=1\r\n";
        let (text, encoding) = detect(bytes).unwrap();
        assert_eq!(
            (text.as_str(), encoding),
            ("\u{feff}name=1\r\n", TextEncoding::Utf8)
        );
        assert_eq!(encode(&text, encoding).unwrap(), bytes);
        assert_eq!(detect(b"plain\n").unwrap().1, TextEncoding::Utf8);
        assert_eq!(detect(b"").unwrap(), (String::new(), TextEncoding::Utf8));
    }

    // 带 BOM 的 UTF-16LE / BE：按 BOM 打开，BOM 留在内容开头，换行与 emoji 原样，写回逐字节相同
    #[test]
    fn utf16_with_bom_round_trips() {
        let text = "第一行 line 1\r\n😀 emoji\r\n";
        for (big_endian, want) in [
            (false, TextEncoding::Utf16Le),
            (true, TextEncoding::Utf16Be),
        ] {
            let bytes = utf16(text, big_endian, true);
            let (content, encoding) = detect(&bytes).unwrap();
            assert_eq!(encoding, want);
            assert_eq!(content, format!("\u{feff}{text}"));
            assert_eq!(encode(&content, encoding).unwrap(), bytes);
            // 只有 BOM 的空文件
            let empty = utf16("", big_endian, true);
            assert_eq!(detect(&empty).unwrap(), ("\u{feff}".to_string(), want));
        }
        // PowerShell 5.1 重定向生成的样子
        let ps = [0xff, 0xfe, b'o', 0, b'k', 0, b'\r', 0, b'\n', 0];
        assert_eq!(detect(&ps).unwrap().0, "\u{feff}ok\r\n");
    }

    // 没有 BOM：以 ASCII 为主的能识别，以汉字为主的识别不了（可以手动选编码打开），保存时也没有 BOM
    #[test]
    fn utf16_without_bom_is_detected_only_when_clear() {
        let text = "Get-Process | Out-File log.txt\r\n进程 1234\r\n";
        for (big_endian, want) in [
            (false, TextEncoding::Utf16Le),
            (true, TextEncoding::Utf16Be),
        ] {
            let bytes = utf16(text, big_endian, false);
            let (content, encoding) = detect(&bytes).unwrap();
            assert_eq!((content.as_str(), encoding), (text, want));
            assert_eq!(encode(&content, encoding).unwrap(), bytes, "不加 BOM");
        }
        let chinese = utf16("全部是汉字的内容，没有英文", false, false);
        assert_eq!(detect(&chinese), None, "判断不了时不自动识别");
        assert_eq!(
            decode(&chinese, TextEncoding::Utf16Le).as_deref(),
            Some("全部是汉字的内容，没有英文"),
            "手动选 UTF-16LE 能打开"
        );
    }

    // 奇数长度、孤立的代理项、字节序反了：不是合法的 UTF-16
    #[test]
    fn invalid_utf16_is_rejected() {
        let mut odd = utf16("abc", false, true);
        odd.push(b'd');
        assert_eq!(detect(&odd), None);
        assert_eq!(decode(&odd, TextEncoding::Utf16Le), None);
        // 只有高位代理（😀 的前一半），后面是普通字符
        let lone_high = [0xff, 0xfe, 0x3d, 0xd8, b'a', 0];
        assert_eq!(detect(&lone_high), None);
        // 只有低位代理
        let lone_low = [b'a', 0, 0x00, 0xde, b'b', 0];
        assert_eq!(decode(&lone_low, TextEncoding::Utf16Le), None);
        // LE 的 BOM 按 BE 解码：开头是 U+FFFE
        let le = utf16("text", false, true);
        assert_eq!(decode(&le, TextEncoding::Utf16Be), None);
        assert!(decode(&le, TextEncoding::Utf16Le).is_some());
        // UTF-16 的内容按 UTF-8、GB18030 解码：含 NUL
        assert_eq!(decode(&le, TextEncoding::Utf8), None);
        assert_eq!(decode(&le, TextEncoding::Gb18030), None);
    }

    // emoji（BMP 之外，代理对）往返：LE 与 BE 的字节都对
    #[test]
    fn emoji_round_trips_as_surrogate_pairs() {
        let text = "😀🎉 ok";
        let le = encode(text, TextEncoding::Utf16Le).unwrap();
        assert_eq!(&le[..4], &[0x3d, 0xd8, 0x00, 0xde]);
        let be = encode(text, TextEncoding::Utf16Be).unwrap();
        assert_eq!(&be[..4], &[0xd8, 0x3d, 0xde, 0x00]);
        assert_eq!(decode(&le, TextEncoding::Utf16Le).as_deref(), Some(text));
        assert_eq!(decode(&be, TextEncoding::Utf16Be).as_deref(), Some(text));
    }

    // 二进制文件不被当成 UTF-16
    #[test]
    fn binary_is_not_mistaken_for_utf16() {
        let elf = [0x7f, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let png = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d,
        ];
        // 一串小的 16 位整数：NUL 都在奇数位，但解出来是控制字符
        let numbers: Vec<u8> = (1u16..=40).flat_map(u16::to_le_bytes).collect();
        // UTF-32LE（BOM 是 FF FE 00 00）
        let utf32: Vec<u8> = "\u{feff}abc"
            .chars()
            .flat_map(|c| (c as u32).to_le_bytes())
            .collect();
        for (name, bytes) in [
            ("ELF", &elf[..]),
            ("PNG", &png[..]),
            ("16 位整数", &numbers[..]),
            ("UTF-32LE", &utf32[..]),
            ("全是 NUL", &[0u8; 64][..]),
        ] {
            assert_eq!(detect(bytes), None, "{name}");
        }
    }
}
