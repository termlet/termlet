//! 远程文件名的编码：服务器上的文件名是 GBK、Big5 等编码时，在 SFTP 客户端与通道之间转换。
//!
//! russh-sftp 以 `String` 收发路径，收到不是 UTF-8 的名称时按 lossy 解码，原始字节就丢了，这样的文件再也打不开、
//! 删不掉。所以不改库，而是在它与通道之间按 SFTP v3 的包格式改写（[`wrap`]）：
//! - 服务器的 NAME 回复（列目录、realpath、readlink）中的名称按所选编码解码成 UTF-8；
//! - 发出的请求（OPEN、STAT、LSTAT、SETSTAT、OPENDIR、REMOVE、MKDIR、RMDIR、REALPATH、RENAME、READLINK、SYMLINK
//!   与 posix-rename 等扩展）中的路径编码回服务器的编码；其余的包（读写的数据等）原样通过；
//! - 解码不了的字节（UTF-8 时是不合法的 UTF-8）写成 U+10FE00 + 字节（[`raw_byte`]）：界面显示为 `<0xNN>`，
//!   原样用在路径中时还原成原来的字节，所以这样的文件也能打开、改名、删除；
//! - 路径中有所选编码表示不了的字符时（Big5 没有的字等）不发给服务器，直接代替服务器回一个失败的 STATUS
//!   （说明为 [`UNENCODABLE`]，报为 [`super::Rejection::UnencodableName`]）。
//!
//! 为什么是 U+10FE00～U+10FEFF：在补充私用区 B（U+100000～U+10FFFD）中，不会与有含义的字符冲突；整段避开了
//! 非字符 U+10FFFE、U+10FFFF（有的程序会拒绝或替换非字符）。真的文件名中出现这一段的字符时，它的 UTF-8 字节
//! 同样逐字节转义，往返仍然不变。

use std::sync::Arc;

use encoding_rs::{BIG5, GB18030};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf,
};
use tokio::sync::{Mutex, watch};

use crate::model::Encoding;

/// 转义字符的起点：字节 b 写成 U+10FE00 + b。
const ESCAPE_BASE: u32 = 0x10FE00;
/// 客户端与转换之间每个方向的缓冲。
const PIPE_BUFFER: usize = 256 * 1024;
/// 包长度的上限（OpenSSH 的 sftp-server 不超过 256 KB 加包头）：超过时按协议错误结束。
const MAX_PACKET: usize = 4 << 20;
/// 路径中有所选编码表示不了的字符时，代替服务器回答的 STATUS 的说明。
pub(crate) const UNENCODABLE: &str = "termlet: name not representable in the selected encoding";

const FXP_OPEN: u8 = 3;
const FXP_LSTAT: u8 = 7;
const FXP_SETSTAT: u8 = 9;
const FXP_OPENDIR: u8 = 11;
const FXP_REMOVE: u8 = 13;
const FXP_MKDIR: u8 = 14;
const FXP_RMDIR: u8 = 15;
const FXP_REALPATH: u8 = 16;
const FXP_STAT: u8 = 17;
const FXP_RENAME: u8 = 18;
const FXP_READLINK: u8 = 19;
const FXP_SYMLINK: u8 = 20;
const FXP_STATUS: u8 = 101;
const FXP_NAME: u8 = 104;
const FXP_EXTENDED: u8 = 200;
const FX_FAILURE: u32 = 4;

/// 转义字符代表的原始字节（`decode_name` 解码不了的字节）；不是转义字符时为 `None`。
pub fn raw_byte(c: char) -> Option<u8> {
    let v = c as u32;
    (ESCAPE_BASE..=ESCAPE_BASE + 0xFF)
        .contains(&v)
        .then(|| (v - ESCAPE_BASE) as u8)
}

fn escape(b: u8) -> char {
    char::from_u32(ESCAPE_BASE + b as u32).expect("补充私用区 B 中的字符")
}

fn escape_all(bytes: &[u8], out: &mut String) {
    out.extend(bytes.iter().map(|&b| escape(b)));
}

/// 服务器给的名称（字节）解码成字符串：解码不了的字节、本来就在转义区间中的字符逐字节转义。
pub(crate) fn decode_name(bytes: &[u8], encoding: Encoding) -> String {
    let mut out = String::with_capacity(bytes.len());
    match encoding {
        Encoding::Utf8 => {
            for chunk in bytes.utf8_chunks() {
                for c in chunk.valid().chars() {
                    if raw_byte(c).is_some() {
                        let mut buf = [0; 4];
                        escape_all(c.encode_utf8(&mut buf).as_bytes(), &mut out);
                    } else {
                        out.push(c);
                    }
                }
                escape_all(chunk.invalid(), &mut out);
            }
        }
        Encoding::Gb18030 | Encoding::Big5 => {
            let mut i = 0;
            while i < bytes.len() {
                let b = bytes[i];
                if b < 0x80 {
                    out.push(b as char);
                    i += 1;
                    continue;
                }
                let len = sequence_len(encoding, &bytes[i..]);
                if len > 0
                    && let Some(text) = decode_one(encoding, &bytes[i..i + len])
                {
                    out.push_str(&text);
                    i += len;
                    continue;
                }
                // 不是这种编码的字符：只转义这一个字节，从下一个字节接着认
                out.push(escape(b));
                i += 1;
            }
        }
    }
    out
}

/// 从 `bytes` 开头的多字节字符的长度（按编码的结构，不查字表）；不像一个字符时为 0。
fn sequence_len(encoding: Encoding, bytes: &[u8]) -> usize {
    let lead = |b: u8| (0x81..=0xFE).contains(&b);
    match (encoding, bytes) {
        // GB18030：双字节的第二个字节为 0x40–0x7E、0x80–0xFE；四字节为 81–FE 30–39 81–FE 30–39
        (Encoding::Gb18030, [b1, b2, rest @ ..]) if lead(*b1) => match b2 {
            0x30..=0x39 => match rest {
                [b3, b4, ..] if lead(*b3) && (0x30..=0x39).contains(b4) => 4,
                _ => 0,
            },
            0x40..=0x7E | 0x80..=0xFE => 2,
            _ => 0,
        },
        // Big5：第二个字节为 0x40–0x7E、0xA1–0xFE
        (Encoding::Big5, [b1, b2, ..]) if lead(*b1) && matches!(b2, 0x40..=0x7E | 0xA1..=0xFE) => 2,
        _ => 0,
    }
}

/// 解码一个字符；字表中没有、或编码回去与原来的字节不同（不能原样往返）时为 `None`。
fn decode_one(encoding: Encoding, bytes: &[u8]) -> Option<String> {
    let codec = match encoding {
        Encoding::Gb18030 => GB18030,
        Encoding::Big5 => BIG5,
        Encoding::Utf8 => return None,
    };
    let text = codec.decode_without_bom_handling_and_without_replacement(bytes)?;
    if text.chars().any(|c| raw_byte(c).is_some()) {
        return None;
    }
    let (back, _, unmappable) = codec.encode(&text);
    (!unmappable && back.as_ref() == bytes).then(|| text.into_owned())
}

/// 名称编码成服务器的编码：转义字符还原成原来的字节；有这种编码表示不了的字符时为 `None`。
pub(crate) fn encode_name(name: &str, encoding: Encoding) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(name.len());
    let mut buf = [0; 4];
    for c in name.chars() {
        if let Some(b) = raw_byte(c) {
            out.push(b);
            continue;
        }
        let utf8 = c.encode_utf8(&mut buf);
        let codec = match encoding {
            _ if c.is_ascii() => None,
            Encoding::Utf8 => None,
            Encoding::Gb18030 => Some(GB18030),
            Encoding::Big5 => Some(BIG5),
        };
        match codec {
            None => out.extend_from_slice(utf8.as_bytes()),
            Some(codec) => {
                let (bytes, _, unmappable) = codec.encode(utf8);
                if unmappable {
                    return None;
                }
                out.extend_from_slice(&bytes);
            }
        }
    }
    Some(out)
}

// ------------------------------------------------------------ 包

/// 按 SFTP 的格式读一个包的内容。越界时为 `None`。
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(bytes)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// 跳过文件属性（按标志位：大小、属主、权限、时间、扩展）。
    fn skip_attrs(&mut self) -> Option<()> {
        let flags = self.u32()?;
        if flags & 0x1 != 0 {
            self.take(8)?;
        }
        if flags & 0x2 != 0 {
            self.take(8)?;
        }
        if flags & 0x4 != 0 {
            self.take(4)?;
        }
        if flags & 0x8 != 0 {
            self.take(8)?;
        }
        if flags & 0x8000_0000 != 0 {
            for _ in 0..self.u32()? {
                self.string()?;
                self.string()?;
            }
        }
        Some(())
    }

    fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }
}

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// 填上包开头的长度。
fn framed(mut packet: Vec<u8>) -> Vec<u8> {
    let len = (packet.len() - 4) as u32;
    packet[..4].copy_from_slice(&len.to_be_bytes());
    packet
}

/// 请求中跟在 ID 后面的路径个数。
fn path_count(kind: u8) -> usize {
    match kind {
        FXP_OPEN | FXP_LSTAT | FXP_SETSTAT | FXP_OPENDIR | FXP_REMOVE | FXP_MKDIR | FXP_RMDIR
        | FXP_REALPATH | FXP_STAT | FXP_READLINK => 1,
        FXP_RENAME | FXP_SYMLINK => 2,
        _ => 0,
    }
}

/// 扩展请求中跟在扩展名后面的路径个数。
fn extended_path_count(name: &[u8]) -> usize {
    match name {
        b"posix-rename@openssh.com" | b"hardlink@openssh.com" => 2,
        b"statvfs@openssh.com" | b"lsetstat@openssh.com" | b"expand-path@openssh.com" => 1,
        _ => 0,
    }
}

/// 改写一个请求包（含长度）中的路径。不用改时为 `Ok(None)`；路径中有表示不了的字符时为请求的 ID。
fn encode_request(packet: &[u8], encoding: Encoding) -> Result<Option<Vec<u8>>, u32> {
    let body = &packet[4..];
    let mut r = Reader::new(body);
    let (Some(kind), Some(id)) = (r.u8(), r.u32()) else {
        return Ok(None);
    };
    let mut count = path_count(kind);
    if kind == FXP_EXTENDED {
        let Some(name) = r.string() else {
            return Ok(None);
        };
        count = extended_path_count(name);
    }
    if count == 0 {
        return Ok(None);
    }
    let prefix = r.pos;
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(path) = r.string() else {
            return Ok(None);
        };
        paths.push(path);
    }
    let mut encoded = Vec::with_capacity(count);
    let mut changed = false;
    for path in paths {
        // russh-sftp 发的路径总是 UTF-8
        let Ok(text) = std::str::from_utf8(path) else {
            return Ok(None);
        };
        let bytes = encode_name(text, encoding).ok_or(id)?;
        changed |= bytes != path;
        encoded.push(bytes);
    }
    if !changed {
        return Ok(None);
    }
    let mut out = Vec::with_capacity(packet.len() + 16);
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&body[..prefix]);
    for bytes in &encoded {
        put_string(&mut out, bytes);
    }
    out.extend_from_slice(r.rest());
    Ok(Some(framed(out)))
}

/// 改写一个回复包（含长度）：NAME 中的名称解码。不用改时为 `None`。
fn decode_response(packet: &[u8], encoding: Encoding) -> Option<Vec<u8>> {
    let body = &packet[4..];
    let mut r = Reader::new(body);
    if r.u8()? != FXP_NAME {
        return None;
    }
    r.u32()?;
    let count = r.u32()?;
    let mut out = Vec::with_capacity(packet.len() + 64);
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&body[..r.pos]);
    let mut changed = false;
    for _ in 0..count {
        let name = r.string()?;
        let long = r.string()?;
        let attrs = r.pos;
        r.skip_attrs()?;
        for text in [name, long] {
            let decoded = decode_name(text, encoding);
            changed |= decoded.as_bytes() != text;
            put_string(&mut out, decoded.as_bytes());
        }
        out.extend_from_slice(&body[attrs..r.pos]);
    }
    if !changed {
        return None;
    }
    out.extend_from_slice(r.rest());
    Some(framed(out))
}

/// 代替服务器回答的失败（[`UNENCODABLE`]）。
fn unencodable_status(id: u32) -> Vec<u8> {
    let mut out = vec![0; 4];
    out.push(FXP_STATUS);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&FX_FAILURE.to_be_bytes());
    put_string(&mut out, UNENCODABLE.as_bytes());
    put_string(&mut out, b"en");
    framed(out)
}

// ------------------------------------------------------------ 字节流

/// 在 `stream`（SFTP 服务端的标准输入输出）与返回的字节流（交给 SFTP 客户端）之间按 `encoding` 转换文件名。
/// 客户端关闭或丢弃它的一端时，转换随即结束并丢弃 `stream`（通道随之关闭）；`stream` 结束时客户端读到结尾。
pub(crate) fn wrap<S>(stream: S, encoding: Encoding) -> DuplexStream
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (client, pump) = tokio::io::duplex(PIPE_BUFFER);
    let (from_server, to_server) = tokio::io::split(stream);
    let (from_client, to_client) = tokio::io::split(pump);
    let to_client = Arc::new(Mutex::new(to_client));
    let (done_tx, done) = watch::channel(false);
    crate::runtime::spawn(requests(
        from_client,
        to_server,
        to_client.clone(),
        encoding,
        done_tx,
    ));
    crate::runtime::spawn(responses(from_server, to_client, encoding, done));
    client
}

/// 读一个完整的包（含 4 字节的长度）到 `buf`。流结束、出错或长度不对时为假。
async fn read_packet<R: AsyncRead + Unpin>(reader: &mut R, buf: &mut Vec<u8>) -> bool {
    let mut len = [0; 4];
    if reader.read_exact(&mut len).await.is_err() {
        return false;
    }
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_PACKET {
        tracing::debug!(len = n, "SFTP 包的长度不对，结束");
        return false;
    }
    buf.clear();
    buf.extend_from_slice(&len);
    buf.resize(4 + n, 0);
    reader.read_exact(&mut buf[4..]).await.is_ok()
}

/// 客户端 → 服务端：改写请求中的路径。客户端关闭时关闭服务端的输入，并让另一个方向结束。
async fn requests<S: AsyncWrite>(
    mut from_client: ReadHalf<DuplexStream>,
    mut to_server: WriteHalf<S>,
    to_client: Arc<Mutex<WriteHalf<DuplexStream>>>,
    encoding: Encoding,
    done: watch::Sender<bool>,
) {
    let mut packet = Vec::new();
    while read_packet(&mut from_client, &mut packet).await {
        let written = match encode_request(&packet, encoding) {
            Ok(None) => to_server.write_all(&packet).await,
            Ok(Some(changed)) => to_server.write_all(&changed).await,
            Err(id) => {
                tracing::debug!(id, "路径中有所选编码表示不了的字符，不发给服务器");
                let reply = unencodable_status(id);
                if to_client.lock().await.write_all(&reply).await.is_err() {
                    break;
                }
                Ok(())
            }
        };
        if written.is_err() || to_server.flush().await.is_err() {
            break;
        }
    }
    let _ = to_server.shutdown().await;
    done.send_replace(true);
}

/// 服务端 → 客户端：改写回复中的名称。服务端结束时客户端读到结尾。
async fn responses<S: AsyncRead>(
    mut from_server: ReadHalf<S>,
    to_client: Arc<Mutex<WriteHalf<DuplexStream>>>,
    encoding: Encoding,
    mut done: watch::Receiver<bool>,
) {
    let mut packet = Vec::new();
    loop {
        let more = tokio::select! {
            more = read_packet(&mut from_server, &mut packet) => more,
            _ = done.wait_for(|done| *done) => false,
        };
        if !more {
            break;
        }
        let changed = decode_response(&packet, encoding);
        let mut writer = to_client.lock().await;
        if writer
            .write_all(changed.as_deref().unwrap_or(&packet))
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = to_client.lock().await.shutdown().await;
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use russh_sftp::protocol::{
        File, FileAttributes, Name, Open, OpenFlags, Packet, Remove, Rename, Write,
    };

    use super::*;

    fn gbk(text: &str) -> Vec<u8> {
        GB18030.encode(text).0.into_owned()
    }

    fn framed_packet(packet: Packet) -> Vec<u8> {
        Bytes::try_from(packet).unwrap().to_vec()
    }

    fn parse(packet: &[u8]) -> Packet {
        Packet::try_from(&mut Bytes::copy_from_slice(&packet[4..])).unwrap()
    }

    /// 把包中的字符串 `from` 换成任意字节 `to`（russh-sftp 只能发 UTF-8 的名称），并改好包的长度。
    fn replace_string(packet: &mut Vec<u8>, from: &str, to: &[u8]) {
        let mut needle = (from.len() as u32).to_be_bytes().to_vec();
        needle.extend_from_slice(from.as_bytes());
        let at = packet
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap();
        let mut with = (to.len() as u32).to_be_bytes().to_vec();
        with.extend_from_slice(to);
        packet.splice(at..at + needle.len(), with);
        let len = (packet.len() - 4) as u32;
        packet[..4].copy_from_slice(&len.to_be_bytes());
    }

    #[test]
    fn escapes_stay_in_private_use_and_avoid_noncharacters() {
        assert_eq!(escape(0x00), '\u{10FE00}');
        assert_eq!(escape(0xFF), '\u{10FEFF}');
        for b in 0..=255u8 {
            assert_eq!(raw_byte(escape(b)), Some(b));
        }
        assert_eq!(raw_byte('a'), None);
        assert_eq!(raw_byte('\u{10FFFE}'), None);
        assert_eq!(raw_byte('\u{10FDFF}'), None);
    }

    #[test]
    fn gbk_and_big5_names_round_trip() {
        for (encoding, text) in [
            (Encoding::Gb18030, "中文目录/报告 2024.xlsx"),
            (Encoding::Gb18030, "emoji 😀 与生僻字 𠀀"),
            (Encoding::Big5, "測試檔案.txt"),
            (Encoding::Utf8, "普通的 UTF-8 名称"),
        ] {
            let bytes = encode_name(text, encoding).unwrap();
            assert_eq!(decode_name(&bytes, encoding), text, "{encoding:?}");
        }
        assert_eq!(gbk("中"), [0xD6, 0xD0]);
        assert_eq!(
            decode_name(&[0xD6, 0xD0, b'.', b'c'], Encoding::Gb18030),
            "中.c"
        );
    }

    // 解码不了的字节转义后显示与操作都用原来的字节；本来就在转义区间中的字符同样往返不变
    #[test]
    fn undecodable_bytes_are_kept() {
        for (encoding, bytes) in [
            // UTF-8 下的 GBK 名称
            (Encoding::Utf8, gbk("中文.txt")),
            (
                Encoding::Gb18030,
                vec![0xFF, b'a', 0x81, b'/', 0x80, 0x81, 0x30, 0x41],
            ),
            (Encoding::Big5, vec![0xA4, 0x40, 0x81, 0x30, 0xFE]),
            (Encoding::Utf8, "a\u{10FE41}b".as_bytes().to_vec()),
        ] {
            let name = decode_name(&bytes, encoding);
            assert_eq!(encode_name(&name, encoding).unwrap(), bytes, "{encoding:?}");
        }
        let name = decode_name(&gbk("中"), Encoding::Utf8);
        assert_eq!(
            name.chars().map(raw_byte).collect::<Vec<_>>(),
            [Some(0xD6), Some(0xD0)]
        );
        // Big5 的“一”是 A4 40
        assert_eq!(decode_name(&[0xA4, 0x40], Encoding::Big5), "一");
    }

    #[test]
    fn unencodable_names_are_rejected() {
        assert_eq!(encode_name("😀", Encoding::Big5), None);
        assert_eq!(encode_name("繁體😀", Encoding::Big5), None);
        assert!(encode_name("😀", Encoding::Gb18030).is_some());
    }

    #[test]
    fn requests_with_paths_are_rewritten() {
        let open = framed_packet(Packet::Open(Open {
            id: 7,
            filename: "/srv/中文.txt".into(),
            pflags: OpenFlags::READ,
            attrs: FileAttributes::empty(),
        }));
        let out = encode_request(&open, Encoding::Gb18030).unwrap().unwrap();
        let mut want = b"/srv/".to_vec();
        want.extend(gbk("中文.txt"));
        let mut r = Reader::new(&out[4..]);
        assert_eq!((r.u8(), r.u32()), (Some(3), Some(7)));
        assert_eq!(r.string(), Some(&want[..]));
        assert_eq!(r.u32(), Some(OpenFlags::READ.bits()), "其余的字段原样保留");
        assert_eq!(
            u32::from_be_bytes(out[..4].try_into().unwrap()) as usize,
            out.len() - 4
        );

        // 改名有两个路径
        let rename = framed_packet(Packet::Rename(Rename {
            id: 8,
            oldpath: "/a/旧".into(),
            newpath: "/a/新".into(),
        }));
        let out = encode_request(&rename, Encoding::Gb18030).unwrap().unwrap();
        let mut r = Reader::new(&out[4..]);
        r.u8();
        r.u32();
        assert_eq!(
            r.string().map(<[u8]>::to_vec),
            Some([b"/a/".to_vec(), gbk("旧")].concat())
        );
        assert_eq!(
            r.string().map(<[u8]>::to_vec),
            Some([b"/a/".to_vec(), gbk("新")].concat())
        );

        // UTF-8 下没有转义字符的不用改；读写的数据原样通过
        assert_eq!(encode_request(&open, Encoding::Utf8), Ok(None));
        let write = framed_packet(Packet::Write(Write {
            id: 9,
            handle: "h".into(),
            offset: 0,
            data: "中文".as_bytes().to_vec(),
        }));
        assert_eq!(encode_request(&write, Encoding::Gb18030), Ok(None));

        // UTF-8 下转义字符还原成原来的字节
        let raw = decode_name(&gbk("中"), Encoding::Utf8);
        let remove = framed_packet(Packet::Remove(Remove {
            id: 10,
            filename: format!("/tmp/{raw}"),
        }));
        let out = encode_request(&remove, Encoding::Utf8).unwrap().unwrap();
        let mut r = Reader::new(&out[4..]);
        r.u8();
        r.u32();
        assert_eq!(
            r.string().map(<[u8]>::to_vec),
            Some([b"/tmp/".to_vec(), gbk("中")].concat())
        );

        // 表示不了：返回请求的 ID
        let bad = framed_packet(Packet::Remove(Remove {
            id: 11,
            filename: "/tmp/😀".into(),
        }));
        assert_eq!(encode_request(&bad, Encoding::Big5), Err(11));
        match parse(&unencodable_status(11)) {
            Packet::Status(status) => {
                assert_eq!(status.id, 11);
                assert_eq!(status.error_message, UNENCODABLE);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn extended_rename_paths_are_rewritten() {
        let mut body = vec![0; 4];
        body.push(FXP_EXTENDED);
        body.extend_from_slice(&5u32.to_be_bytes());
        put_string(&mut body, b"posix-rename@openssh.com");
        put_string(&mut body, "旧".as_bytes());
        put_string(&mut body, "新".as_bytes());
        let out = encode_request(&framed(body), Encoding::Gb18030)
            .unwrap()
            .unwrap();
        let mut r = Reader::new(&out[4..]);
        r.u8();
        r.u32();
        assert_eq!(r.string(), Some(&b"posix-rename@openssh.com"[..]));
        assert_eq!(r.string().map(<[u8]>::to_vec), Some(gbk("旧")));
        assert_eq!(r.string().map(<[u8]>::to_vec), Some(gbk("新")));
        assert!(r.rest().is_empty());
    }

    // 列目录的回复：名称与 ls -l 式描述都解码，各种属性原样保留，russh-sftp 能解析
    #[test]
    fn name_replies_are_decoded() {
        let attrs = FileAttributes {
            size: Some(42),
            uid: Some(1000),
            gid: Some(1000),
            permissions: Some(0o100644),
            atime: Some(1),
            mtime: Some(2),
            ..FileAttributes::empty()
        };
        let mut packet = framed_packet(Packet::Name(Name {
            id: 3,
            files: vec![
                File {
                    filename: "placeholder-1".into(),
                    longname: "placeholder-2".into(),
                    attrs: attrs.clone(),
                },
                File {
                    filename: "plain".into(),
                    longname: "-rw-r--r-- plain".into(),
                    attrs: attrs.clone(),
                },
            ],
        }));
        replace_string(&mut packet, "placeholder-1", &gbk("报告.txt"));
        replace_string(
            &mut packet,
            "placeholder-2",
            &[b"-rw-r--r-- 1 u g 42 ".to_vec(), gbk("报告.txt")].concat(),
        );

        let out = decode_response(&packet, Encoding::Gb18030).unwrap();
        let Packet::Name(name) = parse(&out) else {
            panic!("应为 NAME");
        };
        assert_eq!(name.id, 3);
        assert_eq!(name.files[0].filename, "报告.txt");
        assert_eq!(name.files[0].longname, "-rw-r--r-- 1 u g 42 报告.txt");
        assert_eq!(name.files[0].attrs.size, Some(42));
        assert_eq!(name.files[0].attrs.mtime, Some(2));
        assert_eq!(name.files[1].filename, "plain");
        // 名称都是合法 UTF-8 时不用改
        let utf8 = framed_packet(Packet::Name(Name {
            id: 4,
            files: vec![File {
                filename: "中文".into(),
                longname: "中文".into(),
                attrs,
            }],
        }));
        assert_eq!(decode_response(&utf8, Encoding::Utf8), None);
    }

    // 经过转换的字节流：请求与回复都被改写，客户端关闭后服务端读到结尾
    #[test]
    fn stream_rewrites_both_directions() {
        crate::runtime::runtime().block_on(async {
            let (ours, mut server) = tokio::io::duplex(64 * 1024);
            let mut client = wrap(ours, Encoding::Gb18030);
            let remove = framed_packet(Packet::Remove(Remove {
                id: 1,
                filename: "/中".into(),
            }));
            client.write_all(&remove).await.unwrap();
            let mut got = Vec::new();
            assert!(read_packet(&mut server, &mut got).await);
            let mut want = b"/".to_vec();
            want.extend(gbk("中"));
            let mut r = Reader::new(&got[4..]);
            r.u8();
            r.u32();
            assert_eq!(r.string(), Some(&want[..]));

            let mut reply = framed_packet(Packet::Name(Name {
                id: 1,
                files: vec![File {
                    filename: "name-x".into(),
                    longname: "long-x".into(),
                    attrs: FileAttributes::empty(),
                }],
            }));
            replace_string(&mut reply, "name-x", &gbk("中"));
            server.write_all(&reply).await.unwrap();
            let mut got = Vec::new();
            assert!(read_packet(&mut client, &mut got).await);
            let Packet::Name(name) = parse(&got) else {
                panic!("应为 NAME");
            };
            assert_eq!(name.files[0].filename, "中");

            client.shutdown().await.unwrap();
            let mut rest = Vec::new();
            server.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty(), "客户端关闭后服务端读到结尾");
        });
    }

    // 表示不了的名称不发给服务器，客户端直接收到失败
    #[test]
    fn stream_answers_unencodable_names() {
        crate::runtime::runtime().block_on(async {
            let (ours, mut server) = tokio::io::duplex(64 * 1024);
            let mut client = wrap(ours, Encoding::Big5);
            let bad = framed_packet(Packet::Remove(Remove {
                id: 2,
                filename: "/tmp/😀".into(),
            }));
            client.write_all(&bad).await.unwrap();
            let mut got = Vec::new();
            assert!(read_packet(&mut client, &mut got).await);
            let Packet::Status(status) = parse(&got) else {
                panic!("应为 STATUS");
            };
            assert_eq!((status.id, status.error_message.as_str()), (2, UNENCODABLE));
            client.shutdown().await.unwrap();
            let mut rest = Vec::new();
            server.read_to_end(&mut rest).await.unwrap();
            assert!(rest.is_empty(), "没有发给服务器");
        });
    }
}
