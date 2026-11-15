//! ZModem 接收（对方是 sz）：按 go-zmodem 的 receiver.go 实现。

use std::time::{Duration, Instant};

use super::*;

/// 一个文件的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Done,
    /// 跳过（本机不接收、对方不发）
    Skipped,
    /// 中途出错或会话结束
    Failed,
}

/// 接收时的文件操作。
pub(crate) trait ReceiveFiles {
    /// 接收这个文件吗：`false` 时回复 ZSKIP。接收时准备好写入的目标。
    fn accept(&mut self, info: &FileInfo) -> bool;
    /// 写入当前文件的数据。
    fn write(&mut self, data: &[u8]) -> Result<(), ZError>;
    /// 当前文件已收到的字节数。
    fn progress(&mut self, info: &FileInfo, received: u64);
    fn completed(&mut self, info: &FileInfo, received: u64, outcome: Outcome);
}

/// 数据阶段连续出错（每次清掉缓冲、发一个 ZRPOS）最多几次；收到一个正确的子包就重新计数。
const DATA_RETRY_BUDGET: u32 = 25;
/// 连续收到这么多不对的帧，对方多半不是 ZModem。
const MAX_CONSECUTIVE_ERRORS: u32 = 15;
/// 收尾时等对方的 "OO" 最多多久（没等到也算完成）。
const OVER_AND_OUT_WAIT: Duration = Duration::from_secs(1);

impl Session<'_> {
    /// 接收对方发来的全部文件。
    pub(crate) fn receive(&mut self, h: &mut dyn ReceiveFiles) -> Result<(), ZError> {
        #[derive(PartialEq)]
        enum State {
            Init,
            FileWait,
            FileAccept,
            Data,
            Eof,
            Fin,
        }
        let mut state = State::Init;
        let mut info = FileInfo::default();
        let mut offset: u64 = 0;
        let mut retries = 0u32;
        let mut consecutive = 0u32;
        loop {
            self.check_cancel()?;
            match state {
                State::Init => {
                    self.send_zrinit();
                    state = State::FileWait;
                }
                State::FileWait => {
                    self.r.in_data = false;
                    let hdr = match self.recv_header() {
                        Ok(hdr) => hdr,
                        Err(e) if e.is_fatal() => return Err(e),
                        Err(e) => {
                            consecutive += 1;
                            if consecutive >= MAX_CONSECUTIVE_ERRORS {
                                return Err(ZError::Protocol(format!(
                                    "连续 {consecutive} 次出错，对方可能不是 ZModem：{e}"
                                )));
                            }
                            retries += 1;
                            if retries >= self.cfg.max_retries {
                                return Err(ZError::Protocol(format!(
                                    "等待对方发送文件时重试次数用完：{e}"
                                )));
                            }
                            // 还没有接受文件：回复"准备好了，再发一次"（ZRINIT 而不是 ZNAK）
                            self.send_zrinit();
                            continue;
                        }
                    };
                    consecutive = 0;
                    match hdr.typ {
                        // 对方还在初始化（起始帧重发了）
                        ZRQINIT => self.send_zrinit(),
                        ZSINIT => {
                            if hdr.encoding == ZBIN32 {
                                self.crc32 = true;
                            }
                            // 注意序列：不用
                            let _ = self.recv_subpacket(256)?;
                            if hdr.zf0() & TESCCTL != 0 {
                                self.w.set_escape_all(true);
                            }
                            self.send_hex_header(Header::pos(ZACK, 0));
                        }
                        ZFILE => {
                            if hdr.encoding == ZBIN32 {
                                self.crc32 = true;
                            }
                            let (data, _) = self.recv_subpacket(2048)?;
                            info = parse_file_info(&data)?;
                            state = State::FileAccept;
                        }
                        ZFIN => state = State::Fin,
                        // 不执行对方要求的命令
                        ZCOMMAND => self.send_hex_header(Header::pos(ZCOMPL, 0)),
                        ZFREECNT => self.send_hex_header(Header::pos(ZACK, 0x7fff_ffff)),
                        _ => {
                            consecutive += 1;
                            if consecutive >= MAX_CONSECUTIVE_ERRORS {
                                return Err(ZError::Protocol(format!(
                                    "连续 {consecutive} 次收到意外的帧（{}），对方可能不是 ZModem",
                                    frame_name(hdr.typ)
                                )));
                            }
                        }
                    }
                }
                State::FileAccept => {
                    if !h.accept(&info) {
                        self.send_hex_header(Header::new(ZSKIP));
                        h.completed(&info, 0, Outcome::Skipped);
                        state = State::FileWait;
                        continue;
                    }
                    offset = 0;
                    retries = 0;
                    // ZRPOS 总是用十六进制帧头（与 lrzsz 兼容）
                    self.send_hex_header(Header::pos(ZRPOS, offset));
                    self.r.in_data = true;
                    state = State::Data;
                }
                State::Data => {
                    let hdr = match self.recv_header() {
                        Ok(hdr) => hdr,
                        Err(e) => {
                            if e.is_fatal() {
                                h.completed(&info, offset, Outcome::Failed);
                                return Err(e);
                            }
                            if let Err(e) = self.recover_data(offset, &mut retries) {
                                h.completed(&info, offset, Outcome::Failed);
                                return Err(e);
                            }
                            continue;
                        }
                    };
                    match hdr.typ {
                        ZDATA => {
                            if hdr.encoding == ZBIN32 {
                                self.crc32 = true;
                            }
                            let pos = widen(hdr.position(), offset);
                            if pos > offset && !full(&info, offset) {
                                // 对方从我们还没收到的位置开始：写入只能追加，请它从我们的位置重发
                                self.r.purge();
                                self.send_hex_header(Header::pos(ZRPOS, offset));
                                continue;
                            }
                            // 对方从我们已经收到的位置之前开始（重发）：重叠的部分丢弃，只写新的
                            let mut incoming = pos;
                            match self.receive_data(
                                h,
                                &info,
                                &mut offset,
                                &mut incoming,
                                &mut retries,
                            ) {
                                Ok(()) => {}
                                Err(e) if e.is_fatal() => {
                                    h.completed(&info, offset, Outcome::Failed);
                                    return Err(e);
                                }
                                Err(_) => {
                                    if let Err(e) = self.recover_data(offset, &mut retries) {
                                        h.completed(&info, offset, Outcome::Failed);
                                        return Err(e);
                                    }
                                }
                            }
                        }
                        ZEOF => {
                            let pos = widen(hdr.position(), offset);
                            if pos < offset {
                                // 对方说的结尾在我们已写入的位置之前：写多了，不能回退
                                h.completed(&info, offset, Outcome::Failed);
                                return Err(ZError::Protocol("收到的数据超过了文件的结尾".into()));
                            }
                            // 结尾在我们的位置之后：过早或过时的 ZEOF，不理会，真正的 ZEOF 随后会到；
                            // 已写满声明的大小时是文件在传输中变大了（sz 读到文件结尾为止），按完成处理
                            if pos == offset || full(&info, offset) {
                                state = State::Eof;
                            }
                        }
                        ZNAK => self.send_hex_header(Header::pos(ZRPOS, offset)),
                        ZFILE => {
                            // 我们的 ZRPOS 丢了，对方重发了 ZFILE
                            let _ = self.recv_subpacket(2048);
                            self.send_hex_header(Header::pos(ZRPOS, offset));
                        }
                        ZFIN => {
                            h.completed(&info, offset, Outcome::Failed);
                            state = State::Fin;
                        }
                        ZSKIP => {
                            h.completed(&info, offset, Outcome::Skipped);
                            state = State::FileWait;
                        }
                        ZABORT | ZFERR | ZCAN => {
                            h.completed(&info, offset, Outcome::Failed);
                            return Err(ZError::Aborted);
                        }
                        _ => {}
                    }
                }
                State::Eof => {
                    h.completed(&info, offset, Outcome::Done);
                    // 准备好接收下一个文件
                    self.send_zrinit();
                    state = State::FileWait;
                }
                State::Fin => {
                    self.send_hex_header(Header::new(ZFIN));
                    self.over_and_out();
                    return Ok(());
                }
            }
        }
    }

    /// 读掉对方结束时发的 "OO"（等一会儿；不是 "OO" 的留给终端）。
    fn over_and_out(&mut self) {
        let deadline = Instant::now() + OVER_AND_OUT_WAIT;
        for _ in 0..2 {
            match self.r.read_byte_until(deadline) {
                Some(b'O') => {}
                Some(_) => {
                    self.r.pos -= 1;
                    return;
                }
                None => return,
            }
        }
    }

    /// 数据阶段出错后恢复：清掉缓冲中过时的数据，请对方从我们写到的位置重发。
    fn recover_data(&mut self, offset: u64, retries: &mut u32) -> Result<(), ZError> {
        *retries += 1;
        if *retries > DATA_RETRY_BUDGET {
            return Err(ZError::Protocol("接收数据时重试次数用完".into()));
        }
        self.r.purge();
        self.send_hex_header(Header::pos(ZRPOS, offset));
        Ok(())
    }

    /// 接收一个 ZDATA 之后的数据子包，直到帧结束（ZCRCE、ZCRCW）或出错。
    /// `offset` 是写入的位置（只追加）；`incoming` 是收到的数据流的位置，对方从更早的位置重发时落后于 `offset`，
    /// 其间的数据已写过，丢弃。
    fn receive_data(
        &mut self,
        h: &mut dyn ReceiveFiles,
        info: &FileInfo,
        offset: &mut u64,
        incoming: &mut u64,
        retries: &mut u32,
    ) -> Result<(), ZError> {
        loop {
            self.check_cancel()?;
            let (data, end) = self.recv_subpacket(self.cfg.max_block + 256)?;
            // 正确的子包就是进展（即使全在重叠部分里）
            *retries = 0;
            let mut write = &data[..];
            if *incoming < *offset {
                let overlap = (*offset - *incoming) as usize;
                write = data.get(overlap..).unwrap_or_default();
            }
            *incoming += data.len() as u64;
            // 不超过对方声明的大小：超出的不是文件内容（没被 CRC-16 发现的损坏子包、没识别出的重复数据）
            if info.size > 0 {
                let room = info.size.saturating_sub(*offset);
                if (write.len() as u64) > room {
                    write = &write[..room as usize];
                }
            }
            if !write.is_empty() {
                h.write(write)?;
                *offset += write.len() as u64;
                h.progress(info, *offset);
            }
            match end {
                ZCRCG => {}
                ZCRCQ => self.send_hex_header(Header::pos(ZACK, *incoming)),
                ZCRCW => {
                    self.send_hex_header(Header::pos(ZACK, *incoming));
                    return Ok(());
                }
                // ZCRCE：帧结束，接下来是 ZEOF 或新的 ZDATA
                _ => return Ok(()),
            }
        }
    }

    /// ZRINIT：能全双工、能边收边写盘、能用 CRC-32；缓冲大小 0（对方可以一直发）。
    /// 线路会改写控制字符时要求对方转义全部控制字符（ESCCTL）。
    fn send_zrinit(&mut self) {
        let mut hdr = Header::new(ZRINIT);
        let mut caps = CANFDX | CANOVIO;
        if self.cfg.use_crc32 {
            caps |= CANFC32;
        }
        if self.cfg.escape_ctl {
            caps |= ESCCTL;
        }
        hdr.set_zf0(caps);
        self.send_hex_header(hdr);
    }
}

/// 已写满对方声明的大小（多出的不是文件内容，见 receive_data）。大小为 0 表示对方没有声明。
fn full(info: &FileInfo, offset: u64) -> bool {
    info.size > 0 && offset >= info.size
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termxfer::Protocol;
    use crate::termxfer::tests::{PipeOut, quiet_tracker, read_file, wide_pipe};
    use crate::termxfer::zmodem::files::{self as zfiles, Link};

    /// 等对方的下一个帧头（跳过不是帧头的）。
    fn expect(s: &mut Session<'_>, typ: u8) {
        for _ in 0..20 {
            if let Ok(h) = s.recv_header()
                && h.typ == typ
            {
                return;
            }
        }
        panic!("没有等到 {}", frame_name(typ));
    }

    // lrzsz 的 sz 下载正在增长的文件：读到文件结尾为止，发的比 ZFILE 中声明的多，ZEOF 的位置也更大。
    // 写满声明的大小后按完成处理（不请对方从截断的位置重发，否则一直重发到 sz 自己放弃）。
    #[test]
    fn growing_file_completes_at_declared_size() {
        let tmp = tempfile::tempdir().unwrap();
        let local_in = wide_pipe();
        let remote_in = wide_pipe();
        let local_out = PipeOut::new(remote_in.clone());
        let remote_out = PipeOut::new(local_in.clone());
        let data: Vec<u8> = (0..60u8).collect();
        let peer = std::thread::spawn(move || {
            let mut s = Session::new(&remote_in, &*remote_out, Config::default());
            expect(&mut s, ZRINIT);
            let offer = Offer {
                name: "app.log".into(),
                size: 40,
                mtime: None,
                mode: 0o100644,
                files_left: 1,
                bytes_left: 40,
            };
            s.send_bin_header(Header::new(ZFILE));
            s.send_subpacket(&marshal_file_info(&offer), ZCRCW);
            expect(&mut s, ZRPOS);
            s.send_bin_header(Header::pos(ZDATA, 0));
            s.send_subpacket(&data[..30], ZCRCG);
            s.send_subpacket(&data[30..], ZCRCE);
            s.send_bin_header(Header::pos(ZEOF, data.len() as u64));
            expect(&mut s, ZRINIT);
            s.send_hex_header(Header::new(ZFIN));
            expect(&mut s, ZFIN);
            s.w.raw(b"OO");
            s.w.flush();
        });
        let tracker = quiet_tracker(Protocol::Zmodem, false);
        let link = Link {
            pipe: &local_in,
            out: &*local_out,
            tracker: &tracker,
            escape_ctl: false,
        };
        zfiles::receive(&link, tmp.path()).unwrap();
        peer.join().unwrap();
        assert_eq!(
            read_file(&tmp.path().join("app.log")),
            (0..40u8).collect::<Vec<_>>()
        );
        assert_eq!(tracker.info().files, 1);
    }

    // 线路会改写控制字符（非二进制模式的 Telnet）：ZRINIT 要求对方转义全部控制字符，我方发出的也全部转义
    #[test]
    fn escape_ctl_is_requested_and_used() {
        let pipe = wide_pipe();
        let peer_in = wide_pipe();
        let out = PipeOut::new(peer_in.clone());
        let mut s = Session::new(
            &pipe,
            &*out,
            Config {
                escape_ctl: true,
                ..Config::default()
            },
        );
        s.send_zrinit();
        s.send_subpacket(&[0x0d, 0x00, 0x8d, 0x41], ZCRCE);
        let mut peer = Session::new(&peer_in, &*out, Config::default());
        let h = peer.recv_header().unwrap();
        assert_eq!(h.typ, ZRINIT);
        assert_ne!(h.zf0() & ESCCTL, 0);
        let written = out.written();
        let body = &written[written.len() - 10..];
        assert!(
            !body.iter().any(|&b| b == 0x0d || b == 0x00 || b == 0x8d),
            "控制字符应全部转义：{body:02x?}"
        );
    }
}
