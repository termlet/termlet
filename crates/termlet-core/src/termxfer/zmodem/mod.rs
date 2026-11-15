//! ZModem 协议（与 lrzsz 的 sz / rz 互通）。
//!
//! 按 go-zmodem 实现（MIT License，Copyright (c) 2025 pstroka，https://github.com/xx25/go-zmodem，
//! frame.go、reader.go、writer.go、subpacket.go、escape.go、crc.go、fileinfo.go、receiver.go、sender.go；
//! 许可文本见上级目录的 LICENSE-go-zmodem.txt）。只保留与 lrzsz 互通要用到的部分：
//! 十六进制与二进制（CRC-16 / CRC-32）帧头、数据子包、标准转义；不做 RLE、变长帧头、注意序列与 ZSINIT 的发送。
//!
//! 读写都是同步的，在传输自己的线程上运行：读远端的输出经 [`Pipe`]（带截止时间），写远端的输入经
//! [`TerminalInput::write`]（放进队列，不阻塞）。取消后丢弃协议的写入，只发中止序列（见 [`Writer::flush`]）。

pub(crate) mod files;
pub(crate) mod receiver;
pub(crate) mod sender;

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::Crc;
use termlet_io::TerminalInput;

use super::info::XferError;
use super::pipe::{Pipe, PipeError};

// 帧的编码
pub(crate) const ZPAD: u8 = b'*';
pub(crate) const ZDLE: u8 = 0x18;
pub(crate) const ZBIN: u8 = b'A';
pub(crate) const ZHEX: u8 = b'B';
pub(crate) const ZBIN32: u8 = b'C';
const ZBINR32: u8 = b'D';
const ZVBIN: u8 = b'a';
const ZVHEX: u8 = b'b';
const ZVBIN32: u8 = b'c';
const ZVBINR32: u8 = b'd';

// 帧类型
pub(crate) const ZRQINIT: u8 = 0x00;
pub(crate) const ZRINIT: u8 = 0x01;
pub(crate) const ZSINIT: u8 = 0x02;
pub(crate) const ZACK: u8 = 0x03;
pub(crate) const ZFILE: u8 = 0x04;
pub(crate) const ZSKIP: u8 = 0x05;
pub(crate) const ZNAK: u8 = 0x06;
pub(crate) const ZABORT: u8 = 0x07;
pub(crate) const ZFIN: u8 = 0x08;
pub(crate) const ZRPOS: u8 = 0x09;
pub(crate) const ZDATA: u8 = 0x0a;
pub(crate) const ZEOF: u8 = 0x0b;
pub(crate) const ZFERR: u8 = 0x0c;
pub(crate) const ZCRC: u8 = 0x0d;
pub(crate) const ZCHALLENGE: u8 = 0x0e;
pub(crate) const ZCOMPL: u8 = 0x0f;
pub(crate) const ZCAN: u8 = 0x10;
pub(crate) const ZFREECNT: u8 = 0x11;
pub(crate) const ZCOMMAND: u8 = 0x12;

// 数据子包的结尾（ZDLE 之后的字节）
pub(crate) const ZCRCE: u8 = 0x68;
pub(crate) const ZCRCG: u8 = 0x69;
pub(crate) const ZCRCQ: u8 = 0x6a;
pub(crate) const ZCRCW: u8 = 0x6b;
const ZRUB0: u8 = 0x6c;
const ZRUB1: u8 = 0x6d;

// 接收方的能力（ZRINIT 的 ZF0）
pub(crate) const CANFDX: u8 = 0x01;
pub(crate) const CANOVIO: u8 = 0x02;
pub(crate) const CANFC32: u8 = 0x20;
pub(crate) const ESCCTL: u8 = 0x40;
/// ZSINIT 的 ZF0：发送方要求转义控制字符
pub(crate) const TESCCTL: u8 = 0x40;
/// ZFILE 的 ZF0：二进制传输
pub(crate) const ZCBIN: u8 = 1;

const XON: u8 = 0x11;
const XOFF: u8 = 0x13;
pub(crate) const CAN: u8 = 0x18;

/// 中止序列：10 个 CAN 加 10 个退格（与 lrzsz 的 canit 相同，对方数到 5 个 CAN 即中止）。
pub(crate) const ABORT: [u8; 20] = [
    0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08,
    0x08, 0x08, 0x08, 0x08,
];

/// 会话的参数：8K 数据块（ZedZap，lrzsz 支持）、CRC-32；控制阶段 10 秒、数据阶段 30 秒没有输入算超时。
#[derive(Clone, Debug)]
pub(crate) struct Config {
    pub max_block: usize,
    pub use_crc32: bool,
    pub timeout: Duration,
    pub data_timeout: Duration,
    pub max_retries: u32,
    pub garbage_max: usize,
    /// 线路会改写控制字符（非二进制模式的 Telnet）：我方发出的数据转义全部控制字符，
    /// 接收时在 ZRINIT 中要求对方也这样做（ESCCTL）
    pub escape_ctl: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_block: 8192,
            use_crc32: true,
            timeout: Duration::from_secs(10),
            data_timeout: Duration::from_secs(30),
            max_retries: 10,
            garbage_max: 1200,
            escape_ctl: false,
        }
    }
}

/// ZModem 会话出错的原因。
#[derive(Debug)]
pub(crate) enum ZError {
    /// 读远端输出失败：超时、用户取消、终端断开
    Pipe(PipeError),
    /// 对方发来中止序列（5 个 CAN）
    Aborted,
    /// 收到太多不是 ZModem 的数据
    Garbage,
    /// 帧或子包的格式、校验和不对（可以重试）
    Frame(String),
    /// 不应出现的帧、重试次数用完
    Protocol(String),
    /// 本机文件出错
    Local(XferError),
}

impl ZError {
    /// 不再重试、立即结束会话的错误。
    pub(crate) fn is_fatal(&self) -> bool {
        matches!(
            self,
            ZError::Pipe(PipeError::Cancelled | PipeError::Closed)
                | ZError::Aborted
                | ZError::Local(_)
        )
    }
}

impl From<PipeError> for ZError {
    fn from(e: PipeError) -> Self {
        ZError::Pipe(e)
    }
}

impl fmt::Display for ZError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZError::Pipe(PipeError::Timeout) => f.write_str("等待对方的数据超时"),
            ZError::Pipe(PipeError::Cancelled) => f.write_str("已取消"),
            ZError::Pipe(PipeError::Closed) => f.write_str("终端已断开"),
            ZError::Aborted => f.write_str("对方中止了传输"),
            ZError::Garbage => f.write_str("收到太多不是 ZModem 的数据"),
            ZError::Frame(s) | ZError::Protocol(s) => f.write_str(s),
            ZError::Local(e) => write!(f, "{e}"),
        }
    }
}

pub(crate) fn frame_name(typ: u8) -> &'static str {
    const NAMES: [&str; 20] = [
        "ZRQINIT",
        "ZRINIT",
        "ZSINIT",
        "ZACK",
        "ZFILE",
        "ZSKIP",
        "ZNAK",
        "ZABORT",
        "ZFIN",
        "ZRPOS",
        "ZDATA",
        "ZEOF",
        "ZFERR",
        "ZCRC",
        "ZCHALLENGE",
        "ZCOMPL",
        "ZCAN",
        "ZFREECNT",
        "ZCOMMAND",
        "ZSTDERR",
    ];
    NAMES.get(typ as usize).copied().unwrap_or("UNKNOWN")
}

// ------------------------------------------------------------ CRC

/// CRC-16/XMODEM 的查表（多项式 0x1021）。与 lrzsz 的 crctab 加两个零字节收尾的算法结果相同。
const CRC16_TABLE: [u16; 256] = {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// 接着 `crc` 计算 `data` 的 CRC-16/XMODEM。
pub(crate) fn crc16_update(crc: u16, data: &[u8]) -> u16 {
    data.iter().fold(crc, |crc, &b| {
        (crc << 8) ^ CRC16_TABLE[((crc >> 8) as u8 ^ b) as usize]
    })
}

/// ZModem 帧头与数据子包用的 CRC-16/XMODEM（初值 0）。
pub(crate) fn crc16(data: &[u8]) -> u16 {
    crc16_update(0, data)
}

/// 数据与结尾字节的 CRC-32（IEEE，与 zlib 相同）。
fn crc32_of(data: &[u8], end: Option<u8>) -> u32 {
    let mut crc = Crc::new();
    crc.update(data);
    if let Some(end) = end {
        crc.update(&[end]);
    }
    crc.sum()
}

// ------------------------------------------------------------ 帧头

/// 帧头：类型与 4 个字节的参数（位置按小端，标志 ZF0 在最后一个字节）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Header {
    /// 收到时的编码（ZHEX、ZBIN、ZBIN32）
    pub encoding: u8,
    pub typ: u8,
    pub data: [u8; 4],
}

impl Header {
    pub(crate) fn new(typ: u8) -> Self {
        Self {
            typ,
            ..Self::default()
        }
    }

    /// 带位置的帧头（位置只有 32 位，超过 4 GB 时取低 32 位，见 [`widen`]）。
    pub(crate) fn pos(typ: u8, pos: u64) -> Self {
        Self {
            typ,
            data: (pos as u32).to_le_bytes(),
            ..Self::default()
        }
    }

    pub(crate) fn position(&self) -> u32 {
        u32::from_le_bytes(self.data)
    }

    pub(crate) fn zf0(&self) -> u8 {
        self.data[3]
    }

    pub(crate) fn set_zf0(&mut self, value: u8) {
        self.data[3] = value;
    }
}

/// 帧头中 32 位的位置还原成离 `near` 最近的完整位置（超过 4 GB 的文件）。
pub(crate) fn widen(pos: u32, near: u64) -> u64 {
    let diff = pos.wrapping_sub(near as u32) as i32;
    near.saturating_add_signed(diff as i64)
}

// ------------------------------------------------------------ 文件信息

/// 发来的文件（ZFILE 的数据子包）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct FileInfo {
    /// 原样的文件名（不一定是 UTF-8）
    pub name: Vec<u8>,
    /// 大小，0 表示不知道
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
    /// 包括这个在内还剩几个文件、多少字节（对方给出时）
    pub files_left: u32,
    pub bytes_left: u64,
}

/// 要发送的文件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Offer {
    pub name: String,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub mode: u32,
    pub files_left: u32,
    pub bytes_left: u64,
}

/// 编码 ZFILE 的数据子包：`名称\0大小 修改时间(八进制) 权限(八进制) 0 剩余文件数 剩余字节数\0`。
pub(crate) fn marshal_file_info(offer: &Offer) -> Vec<u8> {
    let name = offer.name.replace('\\', "/");
    let mtime = offer
        .mtime
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut meta = format!("{} {:o} {:o} 0", offer.size, mtime, offer.mode);
    if offer.files_left > 0 {
        meta.push_str(&format!(" {}", offer.files_left));
        if offer.bytes_left > 0 {
            meta.push_str(&format!(" {}", offer.bytes_left));
        }
    }
    let mut out = Vec::with_capacity(name.len() + meta.len() + 2);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(meta.as_bytes());
    out.push(0);
    out
}

/// 解析 ZFILE 的数据子包，名称之后的各项都可以没有。
pub(crate) fn parse_file_info(data: &[u8]) -> Result<FileInfo, ZError> {
    let nul = data
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| ZError::Frame("文件信息缺少结尾的 NUL".into()))?;
    let mut info = FileInfo {
        name: data[..nul].to_vec(),
        ..FileInfo::default()
    };
    let rest = String::from_utf8_lossy(&data[nul + 1..]);
    let rest = rest.trim_end_matches('\0');
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if let Some(size) = fields.first().and_then(|f| f.parse().ok()) {
        info.size = size;
    }
    if let Some(mtime) = fields.get(1).and_then(|f| u64::from_str_radix(f, 8).ok())
        && mtime > 0
    {
        info.mtime = Some(UNIX_EPOCH + Duration::from_secs(mtime));
    }
    if let Some(mode) = fields.get(2).and_then(|f| u32::from_str_radix(f, 8).ok()) {
        info.mode = mode;
    }
    if let Some(left) = fields.get(4).and_then(|f| f.parse().ok()) {
        info.files_left = left;
    }
    if let Some(left) = fields.get(5).and_then(|f| f.parse().ok()) {
        info.bytes_left = left;
    }
    Ok(info)
}

// ------------------------------------------------------------ 读

/// 读远端的输出：缓冲、ZDLE 解码、去掉 XON / XOFF、统计无用数据与 CAN。
pub(crate) struct Reader<'a> {
    pipe: &'a Pipe,
    buf: Vec<u8>,
    pos: usize,
    timeout: Duration,
    data_timeout: Duration,
    /// 正在接收数据子包：用数据阶段的超时
    pub in_data: bool,
    garbage_max: usize,
    can_count: usize,
}

impl<'a> Reader<'a> {
    fn new(pipe: &'a Pipe, cfg: &Config) -> Self {
        Self {
            pipe,
            buf: Vec::new(),
            pos: 0,
            timeout: cfg.timeout,
            data_timeout: cfg.data_timeout,
            in_data: false,
            garbage_max: cfg.garbage_max,
            can_count: 0,
        }
    }

    fn read_byte(&mut self) -> Result<u8, ZError> {
        if self.pos >= self.buf.len() {
            let timeout = if self.in_data {
                self.data_timeout
            } else {
                self.timeout
            };
            self.buf = self.pipe.next(Some(Instant::now() + timeout))?;
            self.pos = 0;
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    /// 读一个字节，最多等到 `deadline`；没有时为 `None`。
    fn read_byte_until(&mut self, deadline: Instant) -> Option<u8> {
        if self.pos >= self.buf.len() {
            self.buf = self.pipe.next(Some(deadline)).ok()?;
            self.pos = 0;
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Some(b)
    }

    /// 读一个字节，去掉流控字符 XON / XOFF（数据中的 XON / XOFF 总是转义的）。
    fn read_byte_strip(&mut self) -> Result<u8, ZError> {
        loop {
            let b = self.read_byte()?;
            if !matches!(b & 0x7f, XON | XOFF) {
                return Ok(b);
            }
        }
    }

    /// 读一个 ZDLE 解码后的字节；遇到子包结尾时第二项是结尾的种类。
    fn zdl_read(&mut self) -> Result<(u8, Option<u8>), ZError> {
        loop {
            let b = self.read_byte_strip()?;
            if b != ZDLE {
                self.can_count = 0;
                return Ok((b, None));
            }
            // ZDLE 与 CAN 是同一个字节
            self.can_count += 1;
            if self.can_count >= 5 {
                return Err(ZError::Aborted);
            }
            let c = self.read_byte_strip()?;
            match c {
                ZCRCE | ZCRCG | ZCRCQ | ZCRCW => {
                    self.can_count = 0;
                    return Ok((0, Some(c)));
                }
                ZRUB0 => {
                    self.can_count = 0;
                    return Ok((0x7f, None));
                }
                ZRUB1 => {
                    self.can_count = 0;
                    return Ok((0xff, None));
                }
                c if c >= 0x40 => {
                    self.can_count = 0;
                    return Ok((c ^ 0x40, None));
                }
                // ZDLE 之后是控制字符：线路噪声，丢弃后读下一个
                c => {
                    if c == CAN {
                        self.can_count += 1;
                        if self.can_count >= 5 {
                            return Err(ZError::Aborted);
                        }
                    }
                }
            }
        }
    }

    /// 读两个十六进制数字（去掉奇偶校验位）。
    fn read_hex(&mut self) -> Result<u8, ZError> {
        let hi = self.read_byte()? & 0x7f;
        let lo = self.read_byte()? & 0x7f;
        match ((hi as char).to_digit(16), (lo as char).to_digit(16)) {
            (Some(h), Some(l)) => Ok((h * 16 + l) as u8),
            _ => Err(ZError::Frame(format!(
                "十六进制帧头中有不是数字的字符：0x{hi:02x} 0x{lo:02x}"
            ))),
        }
    }

    /// 找下一个帧的开头（ZPAD [ZPAD] ZDLE 编码），返回编码；跳过的无用数据超过上限时出错。
    fn scan_for_pad(&mut self) -> Result<u8, ZError> {
        self.can_count = 0;
        let mut garbage = 0usize;
        let max = self.garbage_max;
        let junk = move |garbage: &mut usize| -> Result<(), ZError> {
            *garbage += 1;
            if *garbage > max {
                Err(ZError::Garbage)
            } else {
                Ok(())
            }
        };
        loop {
            let b = self.read_byte()?;
            if b == CAN {
                self.can_count += 1;
                if self.can_count >= 5 {
                    return Err(ZError::Aborted);
                }
                junk(&mut garbage)?;
                continue;
            }
            self.can_count = 0;
            if b != ZPAD {
                junk(&mut garbage)?;
                continue;
            }
            let mut b = self.read_byte()?;
            if b == ZPAD {
                b = self.read_byte()?;
            }
            if b != ZDLE {
                junk(&mut garbage)?;
                continue;
            }
            match self.read_byte()? {
                enc @ (ZBIN | ZHEX | ZBIN32) => return Ok(enc),
                enc @ (ZBINR32 | ZVBIN | ZVHEX | ZVBIN32 | ZVBINR32) => {
                    return Err(ZError::Frame(format!("不支持的帧编码 0x{enc:02x}")));
                }
                _ => junk(&mut garbage)?,
            }
        }
    }

    /// 不等待：取来已收到的输出，看其中有没有帧的开头或 CAN（对方在插话：要求重发、确认、中止）。
    fn poll_pad(&mut self) -> bool {
        while let Some(chunk) = self.pipe.try_next() {
            if self.pos > 0 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            self.buf.extend_from_slice(&chunk);
        }
        self.buf[self.pos..].iter().any(|&b| b == ZPAD || b == CAN)
    }

    fn peek_byte(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    /// 丢掉缓冲中的数据（要求对方重发之前清掉过时的数据）。
    fn purge(&mut self) {
        self.buf.clear();
        self.pos = 0;
    }

    /// 还没处理的数据。
    fn take_rest(&mut self) -> Vec<u8> {
        let rest = self.buf.split_off(self.pos.min(self.buf.len()));
        self.buf.clear();
        self.pos = 0;
        rest
    }
}

// ------------------------------------------------------------ 写

const ESC_SEND: u8 = 0;
const ESC_MUST: u8 = 1;
/// 前一个字节是 '@' 时才转义（防止 Telenet 的 CR-@-CR）
const ESC_AFTER_AT: u8 = 2;

fn escape_table(all: bool) -> [u8; 256] {
    let mut table = [ESC_SEND; 256];
    for b in [ZDLE, 0x10, XON, XOFF, 0x90, 0x91, 0x93, 0x98] {
        table[b as usize] = ESC_MUST;
    }
    table[0x0d] = ESC_AFTER_AT;
    table[0x8d] = ESC_AFTER_AT;
    if all {
        // 转义所有控制字符（0x00–0x1f 与 0x80–0x9f）
        for i in 0..32 {
            if table[i] == ESC_SEND {
                table[i] = ESC_MUST;
            }
            if table[i | 0x80] == ESC_SEND {
                table[i | 0x80] = ESC_MUST;
            }
        }
    }
    table
}

/// 写远端的输入：缓冲、ZDLE 转义。
pub(crate) struct Writer<'a> {
    out: &'a dyn TerminalInput,
    pipe: &'a Pipe,
    buf: Vec<u8>,
    table: [u8; 256],
    last_sent: u8,
}

impl<'a> Writer<'a> {
    fn new(out: &'a dyn TerminalInput, pipe: &'a Pipe) -> Self {
        Self {
            out,
            pipe,
            buf: Vec::with_capacity(16 * 1024),
            table: escape_table(false),
            last_sent: 0,
        }
    }

    fn set_escape_all(&mut self, all: bool) {
        self.table = escape_table(all);
    }

    fn raw(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        if let Some(&b) = data.last() {
            self.last_sent = b;
        }
    }

    fn escaped_byte(&mut self, b: u8) {
        let escape = match self.table[b as usize] {
            ESC_MUST => true,
            ESC_AFTER_AT => self.last_sent & 0x7f == b'@',
            _ => false,
        };
        if escape {
            self.buf.push(ZDLE);
            self.last_sent = b ^ 0x40;
        } else {
            self.last_sent = b;
        }
        self.buf.push(self.last_sent);
    }

    fn escaped(&mut self, data: &[u8]) {
        for &b in data {
            self.escaped_byte(b);
        }
    }

    fn hex(&mut self, b: u8) {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        self.buf.push(DIGITS[(b >> 4) as usize]);
        self.buf.push(DIGITS[(b & 0x0f) as usize]);
    }

    /// 发出缓冲的数据。用户取消后丢弃：读取出错时协议会先发一个 ZRPOS 请对方重发，lrzsz 的 sz 把这个 ZRPOS
    /// 和随后的中止序列一次读进自己的缓冲区，只处理了 ZRPOS，之后按 FIONREAD 判断已无输入，把整个文件发完才看到
    /// 中止序列（实测 60MB 时取消后一直在发）。取消后只发中止序列，sz 读到的第一批就是它。
    fn flush(&mut self) {
        if !self.buf.is_empty() && !self.pipe.is_cancelled() {
            self.out.write(&self.buf);
        }
        self.buf.clear();
    }
}

// ------------------------------------------------------------ 会话

/// 一次 ZModem 会话的协议状态。
pub(crate) struct Session<'a> {
    r: Reader<'a>,
    w: Writer<'a>,
    cfg: Config,
    /// 协商好的 CRC
    crc32: bool,
    /// 对方 ZRINIT 中的能力与缓冲大小（0 表示可以一直发）
    remote_flags: u8,
    remote_window: u64,
}

impl<'a> Session<'a> {
    pub(crate) fn new(pipe: &'a Pipe, out: &'a dyn TerminalInput, cfg: Config) -> Self {
        let mut w = Writer::new(out, pipe);
        w.set_escape_all(cfg.escape_ctl);
        Self {
            r: Reader::new(pipe, &cfg),
            w,
            cfg,
            crc32: false,
            remote_flags: 0,
            remote_window: 0,
        }
    }

    /// 结束：还没处理的远端输出放回去（远端程序退出后的输出，应显示在终端中）。
    pub(crate) fn finish(mut self) {
        let rest = self.r.take_rest();
        self.r.pipe.unread(&rest);
    }

    fn check_cancel(&self) -> Result<(), ZError> {
        if self.r.pipe.is_cancelled() {
            Err(ZError::Pipe(PipeError::Cancelled))
        } else {
            Ok(())
        }
    }

    /// 十六进制帧头：ZPAD ZPAD ZDLE ZHEX 类型与参数 CRC-16（都写成两位小写十六进制）CR LF，除 ZACK、ZFIN 外再加 XON。
    fn send_hex_header(&mut self, h: Header) {
        let mut payload = [0u8; 5];
        payload[0] = h.typ;
        payload[1..].copy_from_slice(&h.data);
        self.w.raw(&[ZPAD, ZPAD, ZDLE, ZHEX]);
        for b in payload {
            self.w.hex(b);
        }
        let crc = crc16(&payload);
        self.w.hex((crc >> 8) as u8);
        self.w.hex(crc as u8);
        // 与 lrzsz 相同：CR 与带最高位的 LF
        self.w.raw(&[0x0d, 0x8a]);
        if h.typ != ZACK && h.typ != ZFIN {
            self.w.raw(&[XON]);
        }
        self.w.flush();
    }

    /// 二进制帧头（CRC-32 协商好时为 ZBIN32）。
    fn send_bin_header(&mut self, h: Header) {
        let mut payload = [0u8; 5];
        payload[0] = h.typ;
        payload[1..].copy_from_slice(&h.data);
        self.w
            .raw(&[ZPAD, ZDLE, if self.crc32 { ZBIN32 } else { ZBIN }]);
        self.w.escaped(&payload);
        if self.crc32 {
            self.w.escaped(&crc32_of(&payload, None).to_le_bytes());
        } else {
            self.w.escaped(&crc16(&payload).to_be_bytes());
        }
        self.w.flush();
    }

    /// 收一个帧头（十六进制、二进制都认）。
    fn recv_header(&mut self) -> Result<Header, ZError> {
        let encoding = self.r.scan_for_pad()?;
        let mut h = match encoding {
            ZHEX => self.recv_hex_header()?,
            ZBIN => self.recv_bin_header(false)?,
            _ => self.recv_bin_header(true)?,
        };
        h.encoding = encoding;
        Ok(h)
    }

    fn recv_hex_header(&mut self) -> Result<Header, ZError> {
        let mut raw = [0u8; 7];
        for b in raw.iter_mut() {
            *b = self.r.read_hex()?;
        }
        let h = Header {
            encoding: ZHEX,
            typ: raw[0],
            data: [raw[1], raw[2], raw[3], raw[4]],
        };
        if crc16(&raw[..5]) != u16::from_be_bytes([raw[5], raw[6]]) {
            return Err(ZError::Frame(format!(
                "{} 的十六进制帧头校验和不对",
                frame_name(h.typ)
            )));
        }
        // 结尾的 CR LF（去掉奇偶校验位）：校验和已经对了，结尾不对也接受（与 lrzsz 的 zgethhdr 相同）
        let mut c = self.r.read_byte()?;
        if c & 0x7f == XON {
            c = self.r.read_byte()?;
        }
        if c & 0x7f == 0x0d {
            let _ = self.r.read_byte()?;
        }
        // 之后可能还有 XON：已经在缓冲中时一并读掉
        if h.typ != ZACK && h.typ != ZFIN && self.r.peek_byte().is_some_and(|b| b & 0x7f == XON) {
            let _ = self.r.read_byte();
        }
        Ok(h)
    }

    fn recv_bin_header(&mut self, crc32: bool) -> Result<Header, ZError> {
        let mut payload = [0u8; 5];
        for b in payload.iter_mut() {
            *b = self.read_header_byte()?;
        }
        let h = Header {
            encoding: if crc32 { ZBIN32 } else { ZBIN },
            typ: payload[0],
            data: [payload[1], payload[2], payload[3], payload[4]],
        };
        let ok = if crc32 {
            let mut crc = [0u8; 4];
            for b in crc.iter_mut() {
                *b = self.read_header_byte()?;
            }
            crc32_of(&payload, None) == u32::from_le_bytes(crc)
        } else {
            let hi = self.read_header_byte()?;
            let lo = self.read_header_byte()?;
            crc16(&payload) == u16::from_be_bytes([hi, lo])
        };
        if !ok {
            return Err(ZError::Frame(format!(
                "{} 的二进制帧头校验和不对",
                frame_name(h.typ)
            )));
        }
        Ok(h)
    }

    fn read_header_byte(&mut self) -> Result<u8, ZError> {
        match self.r.zdl_read()? {
            (b, None) => Ok(b),
            (_, Some(_)) => Err(ZError::Frame("帧头中出现了子包的结尾".into())),
        }
    }

    /// 发一个数据子包：CRC 包括数据与结尾的种类；ZCRCW 之后加 XON（与 lrzsz 相同）。
    fn send_subpacket(&mut self, data: &[u8], end: u8) {
        self.w.escaped(data);
        self.w.raw(&[ZDLE, end]);
        if self.crc32 {
            self.w.escaped(&crc32_of(data, Some(end)).to_le_bytes());
        } else {
            let crc = crc16_update(crc16(data), &[end]);
            self.w.escaped(&crc.to_be_bytes());
        }
        if end == ZCRCW {
            self.w.raw(&[XON]);
        }
        self.w.flush();
    }

    /// 收一个数据子包，返回数据与结尾的种类；超过 `max_len` 时出错。
    fn recv_subpacket(&mut self, max_len: usize) -> Result<(Vec<u8>, u8), ZError> {
        let mut data = Vec::with_capacity(max_len.min(self.cfg.max_block + 16));
        loop {
            let (b, end) = self.r.zdl_read()?;
            let Some(end) = end else {
                if data.len() >= max_len {
                    return Err(ZError::Frame(format!("数据子包超过 {max_len} 字节")));
                }
                data.push(b);
                continue;
            };
            let ok = if self.crc32 {
                let mut crc = [0u8; 4];
                for b in crc.iter_mut() {
                    *b = self.read_header_byte()?;
                }
                crc32_of(&data, Some(end)) == u32::from_le_bytes(crc)
            } else {
                let hi = self.read_header_byte()?;
                let lo = self.read_header_byte()?;
                crc16_update(crc16(&data), &[end]) == u16::from_be_bytes([hi, lo])
            };
            if !ok {
                return Err(ZError::Frame("数据子包的校验和不对".into()));
            }
            return Ok((data, end));
        }
    }
}
