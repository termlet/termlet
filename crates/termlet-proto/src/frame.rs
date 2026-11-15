//! 消息帧：4 字节大端长度，后面是这么长的 JSON（UTF-8）。
//!
//! 这里是同步的读写（命令行工具用）；服务端在异步运行时上按同样的格式读写（[`header`]、[`payload_len`]）。

use std::io::{self, Read, Write};

use serde::Serialize;
use serde::de::DeserializeOwned;

/// 长度前缀的字节数。
pub const HEADER_LEN: usize = 4;

/// 读帧失败的原因。
#[derive(Debug)]
pub enum FrameError {
    /// 对方在帧的开头关闭了连接（正常结束）
    Closed,
    Io(io::Error),
    /// 帧超过上限（长度）
    TooLarge(usize),
    /// 内容不是期望的 JSON
    Json(serde_json::Error),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Closed => f.write_str("连接已关闭"),
            FrameError::Io(e) => write!(f, "读写失败：{e}"),
            FrameError::TooLarge(len) => write!(f, "消息过大（{len} 字节）"),
            FrameError::Json(e) => write!(f, "消息格式错误：{e}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        FrameError::Io(error)
    }
}

/// 长度前缀。
pub fn header(len: usize) -> [u8; HEADER_LEN] {
    u32::try_from(len).unwrap_or(u32::MAX).to_be_bytes()
}

/// 从长度前缀读出内容的长度。
pub fn payload_len(header: [u8; HEADER_LEN]) -> usize {
    u32::from_be_bytes(header) as usize
}

/// 编码一帧（长度前缀 + JSON）。
pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    // serde_json 不转义 < > &：命令中这些字符很多时，请求不会比命令本身大很多
    let json = serde_json::to_vec(value).map_err(io::Error::other)?;
    if json.len() > u32::MAX as usize {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "消息过大"));
    }
    let mut frame = Vec::with_capacity(HEADER_LEN + json.len());
    frame.extend_from_slice(&header(json.len()));
    frame.extend_from_slice(&json);
    Ok(frame)
}

/// 写一帧。
pub fn write<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    writer.write_all(&encode(value)?)?;
    writer.flush()
}

/// 读一帧的内容（不解析）；超过 `max` 字节时报错（不读内容）。
pub fn read_payload<R: Read>(reader: &mut R, max: usize) -> Result<Vec<u8>, FrameError> {
    let mut header = [0u8; HEADER_LEN];
    let mut filled = 0;
    while filled < HEADER_LEN {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Err(FrameError::Closed),
            Ok(0) => return Err(FrameError::Io(io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // Windows 上服务端关闭管道时读到"管道已结束"
            Err(e) if filled == 0 && is_closed(&e) => return Err(FrameError::Closed),
            Err(e) => return Err(FrameError::Io(e)),
        }
    }
    let len = payload_len(header);
    if len > max {
        return Err(FrameError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

/// 读一帧并解析。
pub fn read<R: Read, T: DeserializeOwned>(reader: &mut R, max: usize) -> Result<T, FrameError> {
    let payload = read_payload(reader, max)?;
    serde_json::from_slice(&payload).map_err(FrameError::Json)
}

/// 对方关闭了连接（管道已结束、连接被重置）。
pub fn is_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    ) || error.raw_os_error() == Some(109) // ERROR_BROKEN_PIPE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ExecRequest, Request};

    #[test]
    fn frames_round_trip() {
        let mut buffer = Vec::new();
        write(&mut buffer, &Request::ListHosts { query: "a".into() }).unwrap();
        write(&mut buffer, &Request::ListHosts { query: "b".into() }).unwrap();
        let mut reader = buffer.as_slice();
        let first: Request = read(&mut reader, 1024).unwrap();
        let second: Request = read(&mut reader, 1024).unwrap();
        assert_eq!(first, Request::ListHosts { query: "a".into() });
        assert_eq!(second, Request::ListHosts { query: "b".into() });
        assert!(matches!(
            read::<_, Request>(&mut reader, 1024),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn oversized_and_truncated_frames_are_errors() {
        let frame = encode(&"x".repeat(100)).unwrap();
        assert!(matches!(
            read_payload(&mut frame.as_slice(), 10),
            Err(FrameError::TooLarge(102))
        ));
        assert!(matches!(
            read_payload(&mut &frame[..50], 1000),
            Err(FrameError::Io(_))
        ));
        assert!(matches!(
            read_payload(&mut &frame[..2], 1000),
            Err(FrameError::Io(_))
        ));
    }

    // < > & 不转义，不到 1MB 的脚本不会因为转义超过请求的上限
    #[test]
    fn shell_characters_are_not_escaped() {
        let command = "a <b> && c > /dev/null\n".repeat(1000);
        let frame = encode(&Request::Exec(ExecRequest {
            host: "web-1".into(),
            command: command.clone(),
            ..Default::default()
        }))
        .unwrap();
        let text = String::from_utf8_lossy(&frame);
        assert!(text.contains("a <b> && c > /dev/null"));
        assert!(frame.len() < command.len() + command.len() / 10 + 200);
        let back: Request = read(&mut frame.as_slice(), usize::MAX).unwrap();
        assert!(matches!(back, Request::Exec(r) if r.command == command));
    }
}
