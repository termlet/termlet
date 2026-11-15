//! 检测终端输出中的传输起始序列：ZModem 的 ZRQINIT / ZRINIT 帧头，
//! trzsz 的触发行 `::TRZSZ:TRANSFER:<模式>:<版本>:<唯一 ID>`。
//!
//! 起始序列被拆到两次读取中时，末尾可能是它开头的一段先暂扣（不显示），由下一段或 [`Detector::flush`] 处理。

use std::collections::HashSet;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use memchr::memmem;
use regex::bytes::Regex;

use super::info::Protocol;
use super::zmodem::crc16;

/// 检测到的一次传输。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Trigger {
    pub protocol: Protocol,
    /// 本机发往服务器（远端运行的是 rz / trz）；否则是下载（sz / tsz）
    pub upload: bool,
    /// 上传目录（trz -d）
    pub directory: bool,
    /// trzsz：远端的版本、本次传输的唯一 ID、远端是否是 Windows
    pub version: String,
    pub unique_id: String,
    pub win_server: bool,
}

impl Trigger {
    pub(crate) fn zmodem(upload: bool) -> Self {
        Self {
            protocol: Protocol::Zmodem,
            upload,
            directory: false,
            version: String::new(),
            unique_id: String::new(),
            win_server: false,
        }
    }
}

/// ZModem 十六进制帧头的开头：ZPAD ZPAD ZDLE ZHEX。
const ZMODEM_PREFIX: &[u8] = b"**\x18B";
/// 十六进制帧头到 CRC 为止的长度：前缀 4 + 类型 2 + 参数 8 + CRC 4。
const ZMODEM_HEADER_LEN: usize = 18;
/// 帧类型：ZRQINIT（远端 sz 请求我们接收）与 ZRINIT（远端 rz 准备好接收）。
const ZRQINIT: u8 = 0;
const ZRINIT: u8 = 1;

const TRZSZ_MAGIC: &[u8] = b"::TRZSZ:TRANSFER:";

/// 与 trzsz-go 相同：模式（S 发送、R 接收、D 接收目录）、版本、唯一 ID、隧道端口。
static TRZSZ_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"::TRZSZ:TRANSFER:([SRD]):(\d+\.\d+\.\d+)(:\d+)?(:\d+)?").expect("正则无效")
});
static ZMODEM_FINDER: LazyLock<memmem::Finder<'static>> =
    LazyLock::new(|| memmem::Finder::new(ZMODEM_PREFIX));
static TRZSZ_FINDER: LazyLock<memmem::FinderRev<'static>> =
    LazyLock::new(|| memmem::FinderRev::new(TRZSZ_MAGIC));

/// 暂扣的上限：起始序列被拆到两次读取中时，末尾的一段先不显示。
const MAX_HOLD: usize = 96;

/// 一段输出的检测结果。
#[derive(Debug, Default)]
pub(crate) struct Fed {
    /// 立即显示
    pub show: Vec<u8>,
    /// 检测到的传输；这时 `rest` 是起始序列之后的输出（属于这次传输）
    pub trigger: Option<Trigger>,
    pub rest: Vec<u8>,
}

/// 检测器。由包装终端的转发任务独占使用。
#[derive(Default)]
pub(crate) struct Detector {
    held: Vec<u8>,
    /// 已处理过的 trzsz 唯一 ID：同一触发再次出现（例如重绘、回显）时不再触发
    seen: HashSet<String>,
    /// 这之前不检测 ZModem：中止后远端的 sz 可能在收到中止序列之前重发起始帧，不能再次触发
    quiet_until: Option<Instant>,
}

enum Found {
    At {
        idx: usize,
        end: usize,
        trigger: Trigger,
    },
    /// 没有找到；结尾可能是起始序列的开头时给出可能的开头位置
    No { hold: Option<usize> },
}

impl Detector {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 在 `duration` 内不检测 ZModem（传输取消、失败后调用）。
    pub(crate) fn suppress_zmodem(&mut self, duration: Duration) {
        self.quiet_until = Some(Instant::now() + duration);
    }

    /// 检测一段输出。
    pub(crate) fn feed(&mut self, chunk: Vec<u8>) -> Fed {
        let data = if self.held.is_empty() {
            chunk
        } else {
            let mut data = std::mem::take(&mut self.held);
            data.extend_from_slice(&chunk);
            data
        };
        let quiet = self.quiet_until.is_some_and(|until| Instant::now() < until);
        let zmodem = if quiet {
            Found::No { hold: None }
        } else {
            find_zmodem(&data)
        };
        let trzsz = self.find_trzsz(&data, false);
        // 取最先出现的
        let z_idx = match &zmodem {
            Found::At { idx, .. } => Some(*idx),
            Found::No { .. } => None,
        };
        let t_idx = match &trzsz {
            Found::At { idx, .. } => Some(*idx),
            Found::No { .. } => None,
        };
        let pick_zmodem = match (z_idx, t_idx) {
            (Some(z), Some(t)) => z <= t,
            (Some(_), None) => true,
            _ => false,
        };
        if pick_zmodem && let Found::At { idx, end, trigger } = zmodem {
            return Fed {
                show: data[..idx].to_vec(),
                trigger: Some(trigger),
                rest: data[end..].to_vec(),
            };
        }
        if let Found::At { end, trigger, .. } = trzsz {
            return trzsz_fed(&data, end, trigger);
        }
        let z_hold = match zmodem {
            Found::No { hold } => hold,
            Found::At { .. } => None,
        };
        let t_hold = match trzsz {
            Found::No { hold } => hold,
            Found::At { .. } => None,
        };
        let hold = match (z_hold, t_hold) {
            (Some(a), Some(b)) => a.min(b),
            (a, b) => a.or(b).unwrap_or(data.len()),
        };
        if hold < data.len() && data.len() - hold <= MAX_HOLD {
            self.held = data[hold..].to_vec();
            let mut show = data;
            show.truncate(hold);
            return Fed {
                show,
                ..Fed::default()
            };
        }
        Fed {
            show: data,
            ..Fed::default()
        }
    }

    /// 取回暂扣的输出（终端断开、关闭识别时显示出来）。
    pub(crate) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    /// 暂扣的输出等不到后续：其中的 trzsz 触发行到结尾为止已经完整时开始传输（远端输出触发行后就等回答，
    /// 结尾可能一直等不到别的输出），否则照常显示。
    pub(crate) fn settle(&mut self) -> Fed {
        let data = std::mem::take(&mut self.held);
        if let Found::At { end, trigger, .. } = self.find_trzsz(&data, true) {
            return trzsz_fed(&data, end, trigger);
        }
        Fed {
            show: data,
            ..Fed::default()
        }
    }

    /// 是否有暂扣的输出。
    pub(crate) fn holding(&self) -> bool {
        !self.held.is_empty()
    }

    /// 查找 trzsz 的触发行（trz / tsz 输出的 `::TRZSZ:TRANSFER:<模式>:<版本>:<唯一 ID>[:<端口>]`）；触发行还没有结束时
    /// 暂扣它的开头。判断与 trzsz-go 相同：后面带有 `#CFG:`、`Saved` 等字样的是以前传输的输出（例如 cat 日志），唯一 ID
    /// 重复的也不触发。
    ///
    /// 触发行到第一个不属于它的字节为止（不要求换行）：经过 tmux 时，tmux 按屏幕的变化重新输出，触发行后面可能不是
    /// `\r\n`，而是复位滚动区域、定位光标的控制序列（trz 清屏与输出触发行被 tmux 一次读到时就是这样）。`at_end` 表示
    /// 后面等不到更多输出了，到结尾为止就算完整。
    fn find_trzsz(&mut self, data: &[u8], at_end: bool) -> Found {
        let Some(i) = TRZSZ_FINDER.rfind(data) else {
            // 结尾可能是魔串的开头
            for n in (2..TRZSZ_MAGIC.len()).rev() {
                if data.ends_with(&TRZSZ_MAGIC[..n]) {
                    return Found::No {
                        hold: Some(data.len() - n),
                    };
                }
            }
            return Found::No { hold: None };
        };
        let sub = &data[i..];
        // 魔串之后是模式（S、R、D），再往后只有数字、点与冒号
        let line_end = sub[TRZSZ_MAGIC.len()..]
            .iter()
            .enumerate()
            .position(|(k, &b)| {
                let mode = k == 0 && matches!(b, b'S' | b'R' | b'D');
                !(mode || b.is_ascii_digit() || matches!(b, b'.' | b':'))
            })
            .map(|p| TRZSZ_MAGIC.len() + p);
        let line_end = match line_end {
            Some(end) => end,
            None if at_end => sub.len(),
            // 触发行还没有结束（唯一 ID、端口可能还没收完）
            None => return Found::No { hold: Some(i) },
        };
        let Some(m) = TRZSZ_REGEX.captures(&sub[..line_end]) else {
            return Found::No { hold: None };
        };
        if sub.len() > 40 {
            let tail = &sub[40..];
            for s in ["#CFG:", "Saved", "Cancelled", "Stopped", "Interrupted"] {
                if memmem::find(tail, s.as_bytes()).is_some() {
                    return Found::No { hold: None };
                }
            }
        }
        let unique_id = m
            .get(3)
            .map(|g| String::from_utf8_lossy(&g.as_bytes()[1..]).into_owned())
            .unwrap_or_default();
        if unique_id.len() > 6 {
            if self.seen.contains(&unique_id) {
                return Found::No { hold: None };
            }
            if self.seen.len() > 100 {
                self.seen.clear();
            }
            self.seen.insert(unique_id.clone());
        }
        let mode = m[1][0];
        let end = skip_line_end(data, i + line_end);
        let win_server = unique_id == "1" || (unique_id.len() == 13 && unique_id.ends_with("10"));
        Found::At {
            idx: i,
            end,
            trigger: Trigger {
                protocol: Protocol::Trzsz,
                upload: mode != b'S',
                directory: mode == b'D',
                version: String::from_utf8_lossy(&m[2]).into_owned(),
                unique_id,
                win_server,
            },
        }
    }
}

/// 检测到 trzsz 触发行：触发行（与其后的换行、控制序列）原样显示（远端随后会恢复光标、清掉它），TRZSZ 改为
/// TRZSZGO（外层再有 trzsz 客户端时不会重复触发）；`end` 之后的属于这次传输。
fn trzsz_fed(data: &[u8], end: usize, trigger: Trigger) -> Fed {
    Fed {
        show: replace_all(&data[..end], b"TRZSZ", b"TRZSZGO"),
        trigger: Some(trigger),
        rest: data[end..].to_vec(),
    }
}

/// 触发行后面紧跟的换行与完整的 CSI 控制序列（经过 tmux 时是复位滚动区域、定位光标）的结尾：它们是终端的显示，
/// 一并显示，不交给传输（丢掉的话 tmux 记的光标位置、滚动区域与终端对不上，画面会乱）。协议的行以 `#` 开头。
fn skip_line_end(data: &[u8], mut end: usize) -> usize {
    loop {
        match data.get(end) {
            Some(b'\r' | b'\n') => end += 1,
            Some(0x1b) if data.get(end + 1) == Some(&b'[') => {
                // CSI：参数与中间字节（0x20–0x3F），最后是 0x40–0x7E 的结束字节
                let params = &data[end + 2..];
                let Some(n) = params.iter().position(|b| !(0x20..=0x3f).contains(b)) else {
                    return end;
                };
                if !(0x40..=0x7e).contains(&params[n]) {
                    return end;
                }
                end += 2 + n + 1;
            }
            _ => return end,
        }
    }
}

/// 查找校验正确的 ZRQINIT / ZRINIT 帧头；没有完整的帧头但结尾可能是它的开头时暂扣。
/// 帧头要求完整的十六进制与正确的 CRC，普通文本、cat 二进制文件几乎不会误触发（Termora 只看 3 个字节，容易误触发）。
fn find_zmodem(data: &[u8]) -> Found {
    let mut hold = None;
    let mut from = 0;
    while from < data.len() {
        let Some(i) = ZMODEM_FINDER.find(&data[from..]) else {
            break;
        };
        let i = i + from;
        if data.len() - i < ZMODEM_HEADER_LEN {
            if valid_hex_prefix(&data[i + ZMODEM_PREFIX.len()..]) {
                hold = Some(i);
            }
            break;
        }
        if let Some(typ) = parse_header(&data[i + ZMODEM_PREFIX.len()..i + ZMODEM_HEADER_LEN]) {
            return Found::At {
                idx: i,
                end: i + ZMODEM_HEADER_LEN,
                trigger: Trigger::zmodem(typ == ZRINIT),
            };
        }
        from = i + 1;
    }
    if hold.is_none() {
        // 结尾是 "*"、"**"、"**\x18" 时也暂扣
        for n in (1..ZMODEM_PREFIX.len()).rev() {
            if data.ends_with(&ZMODEM_PREFIX[..n]) {
                hold = Some(data.len() - n);
                break;
            }
        }
    }
    Found::No { hold }
}

/// 不完整的帧头（前缀之后的部分）是否仍可能是 ZRQINIT / ZRINIT。
fn valid_hex_prefix(b: &[u8]) -> bool {
    b.iter().enumerate().all(|(i, &c)| match i {
        0 => c == b'0',
        1 => c == b'0' || c == b'1',
        _ => c.is_ascii_hexdigit(),
    })
}

/// 解析帧头中前缀之后的 14 个十六进制字符：类型 1 字节、参数 4 字节、CRC16 2 字节，
/// 只接受 ZRQINIT、ZRINIT 且 CRC 正确的。
fn parse_header(h: &[u8]) -> Option<u8> {
    let mut raw = [0u8; 7];
    for (i, pair) in h.as_chunks::<2>().0.iter().enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        raw[i] = (hi * 16 + lo) as u8;
    }
    let typ = raw[0];
    if typ != ZRQINIT && typ != ZRINIT {
        return None;
    }
    (crc16(&raw[..5]) == u16::from_be_bytes([raw[5], raw[6]])).then_some(typ)
}

fn replace_all(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    let mut last = 0;
    for i in memmem::find_iter(data, from) {
        out.extend_from_slice(&data[last..i]);
        out.extend_from_slice(to);
        last = i + from.len();
    }
    out.extend_from_slice(&data[last..]);
    out
}
