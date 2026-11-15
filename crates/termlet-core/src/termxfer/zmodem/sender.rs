//! ZModem 发送（对方是 rz）：按 go-zmodem 的 sender.go 实现。
//!
//! 数据块从 256 字节开始，连续成功后加倍到 8K；对方能全双工时每 8 个子包用 ZCRCQ 请对方确认一次并等待确认，
//! 在途的数据因此有上限（写入远端是不阻塞的队列，不这样做会把整个文件读进内存）。

use super::receiver::Outcome;
use super::*;

/// 发送时的文件操作。
pub(crate) trait SendFiles {
    /// 下一个要发送的文件；没有了时为 `None`。
    fn next_file(&mut self) -> Result<Option<Offer>, ZError>;
    /// 读当前文件。
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, ZError>;
    /// 从 `pos` 处接着读（对方要求重发）。
    fn seek(&mut self, pos: u64) -> Result<(), ZError>;
    /// 当前文件前 `len` 字节（0 表示整个文件）的 CRC-32（对方用 ZCRC 询问时）。
    fn crc32(&mut self, len: u64) -> Result<u32, ZError>;
    fn progress(&mut self, offer: &Offer, sent: u64);
    fn completed(&mut self, offer: &Offer, sent: u64, outcome: Outcome);
}

/// 等对方的 ZRINIT 时最多容忍几个多余的 ZFIN。
const MAX_SKIP_FIN: u32 = 2;
/// 对方能全双工时，每隔几个子包请对方确认一次。
const ZCRCQ_INTERVAL: u32 = 8;

/// 发送的进行情况。
struct Run {
    offset: u64,
    block: usize,
    good_blocks: u32,
    unreliable: bool,
    /// 对方要求重发后，下一个子包用 ZCRCW（等确认后再接着发）
    zcrcw_next: bool,
}

impl Run {
    /// 对方要求从 `pos` 重发：数据块减小，之后要更多次成功才再加大。
    fn reposition(&mut self, h: &mut dyn SendFiles, pos: u64) -> Result<(), ZError> {
        h.seek(pos)?;
        self.offset = pos;
        self.block = (self.block / 4).max(32);
        self.good_blocks = 0;
        self.unreliable = true;
        self.zcrcw_next = true;
        Ok(())
    }
}

impl Session<'_> {
    /// 把文件逐个发给对方。
    pub(crate) fn send(&mut self, h: &mut dyn SendFiles) -> Result<(), ZError> {
        enum State {
            Init,
            NextFile,
            FileInfo,
            FileInfoAck,
            Data,
            Eof,
            EofAck,
            Fin,
            FinAck,
        }
        let mut state = State::Init;
        let mut offer = Offer::default();
        let mut retries = 0u32;
        let mut skip_fin = 0u32;
        let mut late_zacks = 0u32;
        let mut run = Run {
            offset: 0,
            block: 256,
            good_blocks: 0,
            unreliable: false,
            zcrcw_next: false,
        };
        loop {
            self.check_cancel()?;
            match state {
                State::Init => {
                    // 对方（rz）已经发过 ZRINIT（被检测时读走了）：请它再发一次，带上它的能力
                    self.send_hex_header(Header::new(ZRQINIT));
                    let hdr = self.recv_header_retry(&mut retries)?;
                    match hdr.typ {
                        ZRINIT => {
                            self.process_zrinit(hdr);
                            state = State::NextFile;
                        }
                        ZCHALLENGE => {
                            self.send_hex_header(Header::pos(ZACK, hdr.position() as u64))
                        }
                        ZFIN => {
                            skip_fin += 1;
                            if skip_fin > MAX_SKIP_FIN {
                                return Err(ZError::Protocol(
                                    "等待对方准备接收时一直收到 ZFIN".into(),
                                ));
                            }
                        }
                        typ => {
                            return Err(ZError::Protocol(format!(
                                "应为 ZRINIT，收到 {}",
                                frame_name(typ)
                            )));
                        }
                    }
                }
                State::NextFile => match h.next_file()? {
                    None => state = State::Fin,
                    Some(next) => {
                        offer = next;
                        run.offset = 0;
                        run.good_blocks = 0;
                        run.zcrcw_next = false;
                        retries = 0;
                        late_zacks = 0;
                        state = State::FileInfo;
                    }
                },
                State::FileInfo => {
                    let mut hdr = Header::new(ZFILE);
                    hdr.set_zf0(ZCBIN);
                    self.send_bin_header(hdr);
                    self.send_subpacket(&marshal_file_info(&offer), ZCRCW);
                    state = State::FileInfoAck;
                }
                State::FileInfoAck => {
                    let hdr = self.recv_header_retry(&mut retries)?;
                    match hdr.typ {
                        ZRPOS => {
                            let mut pos = hdr.position() as u64;
                            if pos > offer.size {
                                pos = 0;
                            }
                            if pos > 0 {
                                h.seek(pos)?;
                            }
                            run.offset = pos;
                            state = State::Data;
                        }
                        // 对方不要（已有同名文件且没加 -y）、建不了文件
                        ZSKIP | ZFERR => {
                            h.completed(&offer, 0, Outcome::Skipped);
                            state = State::NextFile;
                        }
                        ZCRC => {
                            let crc = h.crc32(hdr.position() as u64)?;
                            self.send_hex_header(Header::pos(ZCRC, crc as u64));
                        }
                        // 对方对 ZRQINIT 多回了一个 ZRINIT
                        ZRINIT => self.process_zrinit(hdr),
                        ZNAK => {
                            retries += 1;
                            state = State::FileInfo;
                        }
                        typ => {
                            return Err(ZError::Protocol(format!(
                                "应为 ZRPOS 或 ZSKIP，收到 {}",
                                frame_name(typ)
                            )));
                        }
                    }
                }
                State::Data => {
                    state = match self.send_data(h, &offer, &mut run)? {
                        SendNext::Data => State::Data,
                        SendNext::Eof => State::Eof,
                    };
                }
                State::Eof => {
                    self.send_hex_header(Header::pos(ZEOF, run.offset));
                    state = State::EofAck;
                }
                State::EofAck => {
                    let hdr = self.recv_header_retry(&mut retries)?;
                    match hdr.typ {
                        ZRINIT => {
                            h.completed(&offer, run.offset, Outcome::Done);
                            self.process_zrinit(hdr);
                            state = State::NextFile;
                        }
                        ZRPOS => {
                            let pos = widen(hdr.position(), run.offset);
                            run.reposition(h, pos)?;
                            state = State::Data;
                        }
                        ZNAK => {
                            retries += 1;
                            state = State::Eof;
                        }
                        ZSKIP => {
                            h.completed(&offer, run.offset, Outcome::Skipped);
                            state = State::NextFile;
                        }
                        // 之前的 ZCRCQ 的确认晚到了：继续等 ZRINIT
                        ZACK => {
                            late_zacks += 1;
                            if late_zacks > self.cfg.max_retries {
                                return Err(ZError::Protocol(
                                    "发送 ZEOF 后一直收到 ZACK，没有 ZRINIT".into(),
                                ));
                            }
                        }
                        typ => {
                            return Err(ZError::Protocol(format!(
                                "发送 ZEOF 后应为 ZRINIT，收到 {}",
                                frame_name(typ)
                            )));
                        }
                    }
                }
                State::Fin => {
                    self.send_hex_header(Header::new(ZFIN));
                    state = State::FinAck;
                }
                State::FinAck => {
                    // 一直没有回应：当作已完成
                    if retries >= self.cfg.max_retries {
                        return Ok(());
                    }
                    match self.recv_header() {
                        Ok(hdr) if hdr.typ == ZFIN => {
                            // 结束语 "Over and Out"
                            self.w.raw(b"OO");
                            self.w.flush();
                            return Ok(());
                        }
                        Ok(hdr) if hdr.typ == ZNAK => {
                            retries += 1;
                            state = State::Fin;
                        }
                        // 我们发 ZFIN 时对方已经发出的帧（例如 ZRINIT）：接着读，不重发
                        Ok(_) => retries += 1,
                        Err(e) if e.is_fatal() => return Err(e),
                        // 没有回应：与 sz 相同，重发 ZFIN
                        Err(_) => {
                            retries += 1;
                            state = State::Fin;
                        }
                    }
                }
            }
        }
    }

    /// 发一个 ZDATA 帧（直到文件结束或对方要求重发），返回接下来的状态。
    fn send_data(
        &mut self,
        h: &mut dyn SendFiles,
        offer: &Offer,
        run: &mut Run,
    ) -> Result<SendNext, ZError> {
        self.send_bin_header(Header::pos(ZDATA, run.offset));
        let mut buf = vec![0u8; self.cfg.max_block];
        let mut last_ack = run.offset;
        let mut count = 0u32;
        let can_fdx = self.remote_flags & CANFDX != 0;
        loop {
            self.check_cancel()?;
            // 反向通道：对方插话（要求重发、确认、中止）
            if self.r.poll_pad() {
                match self.recv_header() {
                    Ok(hdr) if hdr.typ == ZRPOS => {
                        let pos = widen(hdr.position(), run.offset);
                        run.reposition(h, pos)?;
                        return Ok(SendNext::Data);
                    }
                    Ok(hdr) if hdr.typ == ZACK => last_ack = widen(hdr.position(), run.offset),
                    Ok(_) => {}
                    Err(e) if e.is_fatal() => return Err(e),
                    Err(_) => {}
                }
            }
            // 对方的缓冲有限：在途的数据到了上限就请它确认
            if self.remote_window > 0 && run.offset - last_ack >= self.remote_window {
                let end = if can_fdx { ZCRCQ } else { ZCRCW };
                self.send_subpacket(&[], end);
                let mut tries = 0;
                loop {
                    match self.recv_header() {
                        Ok(hdr) if hdr.typ == ZACK => {
                            last_ack = widen(hdr.position(), run.offset);
                            if end == ZCRCW {
                                // ZCRCW 结束了这个帧：重新发 ZDATA
                                return Ok(SendNext::Data);
                            }
                        }
                        Ok(hdr) if hdr.typ == ZRPOS => {
                            let pos = widen(hdr.position(), run.offset);
                            run.reposition(h, pos)?;
                            return Ok(SendNext::Data);
                        }
                        Ok(_) if end == ZCRCW => return Ok(SendNext::Data),
                        Ok(_) => {}
                        Err(e) if e.is_fatal() => return Err(e),
                        Err(e) => {
                            tries += 1;
                            if tries >= self.cfg.max_retries {
                                return Err(ZError::Protocol(format!(
                                    "等待对方确认时重试次数用完：{e}"
                                )));
                            }
                            self.send_subpacket(&[], end);
                            continue;
                        }
                    }
                    break;
                }
                if run.offset - last_ack >= self.remote_window {
                    continue;
                }
            }
            // 不超过声明的大小：文件在传输中变大时多出的部分不发（对方按声明的大小截断，多发会对不上 ZEOF）
            let want = (offer.size.saturating_sub(run.offset)).min(run.block as u64) as usize;
            let n = if want > 0 {
                h.read(&mut buf[..want])?
            } else {
                0
            };
            if n == 0 {
                // 文件结束（或变小了）：用空的 ZCRCE 子包结束这个帧
                self.send_subpacket(&[], ZCRCE);
                return Ok(SendNext::Eof);
            }
            let at_eof = run.offset + n as u64 >= offer.size;
            let end = if run.zcrcw_next {
                ZCRCW
            } else if at_eof {
                ZCRCE
            } else if can_fdx && count > 0 && count.is_multiple_of(ZCRCQ_INTERVAL) {
                ZCRCQ
            } else {
                ZCRCG
            };
            self.send_subpacket(&buf[..n], end);
            run.offset += n as u64;
            count += 1;
            run.good_blocks += 1;
            if end == ZCRCW {
                // 等对方确认到这里，再用新的 ZDATA 接着发
                let mut tries = 0;
                loop {
                    match self.recv_header() {
                        Ok(hdr) if hdr.typ == ZACK => {
                            if widen(hdr.position(), run.offset) != run.offset {
                                // 位置不对的确认（过时的）：不理会
                                tries += 1;
                                if tries >= self.cfg.max_retries {
                                    return Err(ZError::Protocol("一直收到位置不对的确认".into()));
                                }
                                continue;
                            }
                            run.zcrcw_next = false;
                        }
                        Ok(hdr) if hdr.typ == ZRPOS => {
                            let pos = widen(hdr.position(), run.offset);
                            run.reposition(h, pos)?;
                        }
                        Ok(_) => {
                            tries += 1;
                            if tries >= self.cfg.max_retries {
                                return Err(ZError::Protocol("等待对方确认时收到意外的帧".into()));
                            }
                            continue;
                        }
                        Err(e) if e.is_fatal() => return Err(e),
                        Err(e) => {
                            tries += 1;
                            if tries >= self.cfg.max_retries {
                                return Err(ZError::Protocol(format!(
                                    "等待对方确认时重试次数用完：{e}"
                                )));
                            }
                            continue;
                        }
                    }
                    break;
                }
                h.progress(offer, run.offset);
                return Ok(SendNext::Data);
            }
            if end == ZCRCQ {
                let mut tries = 0;
                loop {
                    match self.recv_header() {
                        Ok(hdr) if hdr.typ == ZACK => last_ack = widen(hdr.position(), run.offset),
                        Ok(hdr) if hdr.typ == ZRPOS => {
                            let pos = widen(hdr.position(), run.offset);
                            run.reposition(h, pos)?;
                            return Ok(SendNext::Data);
                        }
                        Ok(_) => {}
                        Err(e) if e.is_fatal() => return Err(e),
                        Err(e) => {
                            tries += 1;
                            if tries >= self.cfg.max_retries {
                                return Err(ZError::Protocol(format!(
                                    "等待对方确认时重试次数用完：{e}"
                                )));
                            }
                            // 再请一次
                            self.send_subpacket(&[], ZCRCQ);
                            continue;
                        }
                    }
                    break;
                }
            }
            // 连续成功后加大数据块
            let needed = if run.unreliable { 16 } else { 8 };
            if run.good_blocks >= needed && run.block < self.cfg.max_block {
                run.block = (run.block * 2).min(self.cfg.max_block);
                run.good_blocks = 0;
            }
            h.progress(offer, run.offset);
            if at_eof {
                return Ok(SendNext::Eof);
            }
        }
    }

    /// 读帧头，出错（超时、校验和不对）时重试，次数用完时失败。
    fn recv_header_retry(&mut self, retries: &mut u32) -> Result<Header, ZError> {
        loop {
            if *retries >= self.cfg.max_retries {
                return Err(ZError::Protocol(format!(
                    "重试 {} 次仍没有收到对方的回应",
                    self.cfg.max_retries
                )));
            }
            self.check_cancel()?;
            match self.recv_header() {
                Ok(hdr) => return Ok(hdr),
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => {
                    *retries += 1;
                    if *retries >= self.cfg.max_retries {
                        return Err(match e {
                            ZError::Pipe(PipeError::Timeout) => e,
                            e => ZError::Protocol(format!("重试次数用完：{e}")),
                        });
                    }
                }
            }
        }
    }

    /// 对方的 ZRINIT：能力（全双工、CRC-32、要求转义控制字符）与缓冲大小。
    fn process_zrinit(&mut self, hdr: Header) {
        self.remote_flags = hdr.zf0();
        self.remote_window = u64::from(hdr.data[0]) | (u64::from(hdr.data[1]) << 8);
        if self.cfg.use_crc32 && self.remote_flags & CANFC32 != 0 {
            self.crc32 = true;
        }
        if self.remote_flags & ESCCTL != 0 {
            self.w.set_escape_all(true);
        }
    }
}

/// 发完一个 ZDATA 帧之后做什么。
enum SendNext {
    /// 再发一个 ZDATA 帧（对方要求重发、ZCRCW 之后）
    Data,
    /// 文件发完了，发 ZEOF
    Eof,
}
