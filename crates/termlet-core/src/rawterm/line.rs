//! 行规程：编码转换、回车键与退格键的映射、本地回显、自动登录、传输文件时的原样收发。
//! 不做 I/O，读写两边各自加锁调用。

use std::time::Instant;

use zeroize::Zeroizing;

use super::RawConfig;
use super::login::{AutoLogin, Reply};
use crate::charset::{InputEncoder, OutputDecoder};
use crate::model::{Backspace, Encoding, Enter, LocalEcho};

pub(super) struct Discipline {
    enter: Enter,
    backspace: Backspace,
    local_echo: LocalEcho,
    encoding: Encoding,
    decoder: OutputDecoder,
    encoder: InputEncoder,
    login: Option<AutoLogin>,
    echo: EchoFilter,
    /// 在终端中传输文件（ZModem、trzsz）：数据原样收发，不转换编码、不映射回车、不回显、不检查登录提示
    binary: bool,
}

impl Discipline {
    pub(super) fn new(config: &RawConfig, now: Instant) -> Self {
        Self {
            enter: config.enter,
            backspace: config.backspace,
            local_echo: config.local_echo,
            encoding: config.encoding,
            decoder: OutputDecoder::new(config.encoding),
            encoder: InputEncoder::new(config.encoding),
            login: AutoLogin::new(&config.username, config.password.as_ref(), now),
            echo: EchoFilter::default(),
            binary: false,
        }
    }

    /// 对方送来的数据：返回给终端的输出（UTF-8），与自动登录要发给对方的内容（已映射回车、转换编码；
    /// 可能是密码，丢弃时清零）。
    pub(super) fn receive(
        &mut self,
        data: &[u8],
        now: Instant,
    ) -> (Vec<u8>, Option<Zeroizing<Vec<u8>>>) {
        if self.binary {
            return (data.to_vec(), None);
        }
        let out = self.decoder.decode(data);
        let mut reply = None;
        if let Some(login) = &mut self.login
            && !out.is_empty()
        {
            let (answer, done) = login.feed(&out, now);
            if done {
                self.login = None;
            }
            if let Some(answer) = answer {
                // 只记填了什么，不记内容
                let what = match &answer {
                    Reply::Username(_) => "用户名",
                    Reply::Password(_) => "密码",
                };
                tracing::debug!(what, done, "自动登录：看到提示，填写");
                reply = Some(self.encode_line(answer));
            }
        }
        (out, reply)
    }

    /// 自动登录填写的一行（加回车）。中间的每一份副本都清零。
    fn encode_line(&mut self, answer: Reply) -> Zeroizing<Vec<u8>> {
        let mut line = Zeroizing::new(answer.text().as_bytes().to_vec());
        line.push(b'\r');
        let mapped = Zeroizing::new(map_keys(&line, self.enter, self.backspace == Backspace::Bs));
        Zeroizing::new(self.encoder.encode(&mapped))
    }

    /// 用户的输入（UTF-8）：返回本地回显（给终端）与发给对方的数据。`remote_echo` 是对方是否回显输入。
    pub(super) fn send(&mut self, input: &[u8], remote_echo: bool) -> (Vec<u8>, Vec<u8>) {
        if self.binary {
            return (Vec::new(), input.to_vec());
        }
        let echo = match self.local_echo {
            LocalEcho::On => true,
            LocalEcho::Off => false,
            LocalEcho::Auto => !remote_echo,
        };
        let echoed = if echo {
            self.echo.filter(input)
        } else {
            Vec::new()
        };
        (echoed, self.encode(input))
    }

    fn encode(&mut self, input: &[u8]) -> Vec<u8> {
        let mapped = map_keys(input, self.enter, self.backspace == Backspace::Bs);
        self.encoder.encode(&mapped)
    }

    /// 切换对方的编码：之后收到的数据按新的编码解码（已经解码的不变）。
    pub(super) fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = encoding;
        self.decoder = OutputDecoder::new(encoding);
        self.encoder = InputEncoder::new(encoding);
    }

    /// 开始、结束原样收发；丢掉解码器里留着的不完整字符。
    pub(super) fn set_binary(&mut self, on: bool) {
        self.binary = on;
        self.decoder = OutputDecoder::new(self.encoding);
        self.encoder = InputEncoder::new(self.encoding);
        self.echo = EchoFilter::default();
    }
}

/// 把回车键（终端发送 `\r`）映射为 CR LF、CR 或 LF；`bs` 时把退格键（终端发送 0x7F）换成 0x08。
pub(super) fn map_keys(input: &[u8], enter: Enter, bs: bool) -> Vec<u8> {
    let with: Option<&[u8]> = match enter {
        Enter::Crlf => Some(b"\r\n"),
        Enter::Lf => Some(b"\n"),
        Enter::Cr => None,
    };
    if with.is_none() && !bs {
        return input.to_vec();
    }
    let mut out = Vec::with_capacity(input.len() + 8);
    for &b in input {
        match (b, with) {
            (b'\r', Some(with)) => out.extend_from_slice(with),
            (0x7f, _) if bs => out.push(0x08),
            _ => out.push(b),
        }
    }
    out
}

/// 本地回显：可显示的字符原样显示，回车换行，退格擦掉前一个字符，方向键等转义序列与其他控制字符不显示。
#[derive(Default)]
pub(super) struct EchoFilter {
    state: EchoState,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum EchoState {
    #[default]
    Normal,
    /// 收到 ESC
    Escape,
    /// 在 CSI 序列中
    Csi,
    /// 在 SS3 序列中（ESC O 后的一个字符）
    Ss3,
}

impl EchoFilter {
    pub(super) fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for &b in input {
            match self.state {
                EchoState::Escape => {
                    self.state = match b {
                        b'[' => EchoState::Csi,
                        b'O' => EchoState::Ss3,
                        _ => EchoState::Normal,
                    };
                    continue;
                }
                EchoState::Csi => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = EchoState::Normal;
                    }
                    continue;
                }
                EchoState::Ss3 => {
                    self.state = EchoState::Normal;
                    continue;
                }
                EchoState::Normal => {}
            }
            match b {
                0x1b => self.state = EchoState::Escape,
                b'\r' | b'\n' => out.extend_from_slice(b"\r\n"),
                0x7f | 0x08 => out.extend_from_slice(b"\x08 \x08"),
                b'\t' => out.push(b),
                b if b >= 0x20 => out.push(b),
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::SecretString;

    fn config(enter: Enter, local_echo: LocalEcho) -> RawConfig {
        RawConfig {
            enter,
            local_echo,
            ..Default::default()
        }
    }

    #[test]
    fn enter_mapping() {
        for (enter, want) in [
            (Enter::Crlf, &b"a\r\nb\r\n"[..]),
            (Enter::Cr, b"a\rb\r"),
            (Enter::Lf, b"a\nb\n"),
        ] {
            let mut line = Discipline::new(&config(enter, LocalEcho::Off), Instant::now());
            assert_eq!(line.send(b"a\rb\r", true).1, want, "{enter:?}");
        }
    }

    #[test]
    fn backspace() {
        let mut line = Discipline::new(
            &RawConfig {
                enter: Enter::Cr,
                local_echo: LocalEcho::Off,
                backspace: Backspace::Bs,
                ..Default::default()
            },
            Instant::now(),
        );
        assert_eq!(line.send(b"ab\x7f\r", true).1, b"ab\x08\r");
    }

    #[test]
    fn local_echo() {
        let mut line = Discipline::new(&config(Enter::Cr, LocalEcho::Auto), Instant::now());
        // 对方不回显：本地回显可显示的字符、回车换行、退格；方向键（转义序列）不显示
        let (echo, sent) = line.send(b"ab\x7f\x1b[Ac\r", false);
        assert_eq!(echo, b"ab\x08 \x08c\r\n");
        assert_eq!(sent, b"ab\x7f\x1b[Ac\r");
        // 对方回显时"自动"不回显
        assert!(line.send(b"x", true).0.is_empty());
        // SS3 的方向键、拆在两次输入之间的转义序列
        let mut line = Discipline::new(&config(Enter::Cr, LocalEcho::On), Instant::now());
        assert_eq!(line.send(b"\x1bOA\x1b", true).0, b"");
        assert_eq!(line.send(b"[1;5Cz\t\x01", true).0, b"z\t");
        let mut off = Discipline::new(&config(Enter::Cr, LocalEcho::Off), Instant::now());
        assert!(off.send(b"x", false).0.is_empty());
    }

    #[test]
    fn encoding_split_across_chunks() {
        let mut line = Discipline::new(
            &RawConfig {
                encoding: Encoding::Gb18030,
                enter: Enter::Crlf,
                local_echo: LocalEcho::Off,
                ..Default::default()
            },
            Instant::now(),
        );
        // “交换机>” 的 GBK 编码，多字节字符被拆在两块中
        let data = b"\xbd\xbb\xbb\xbb\xbb\xfa>";
        let mut out = line.receive(&data[..3], Instant::now()).0;
        out.extend(line.receive(&data[3..], Instant::now()).0);
        assert_eq!(String::from_utf8(out).unwrap(), "交换机>");
        // “显示” + 回车
        assert_eq!(
            line.send("显示\r".as_bytes(), true).1,
            b"\xcf\xd4\xca\xbe\r\n"
        );
    }

    // 传输文件期间原样收发
    #[test]
    fn binary_mode() {
        let mut line = Discipline::new(
            &RawConfig {
                encoding: Encoding::Gb18030,
                enter: Enter::Crlf,
                local_echo: LocalEcho::On,
                ..Default::default()
            },
            Instant::now(),
        );
        line.set_binary(true);
        let raw = [0x18, b'B', 0xd6, 0xd0, b'\r', 0xff];
        assert_eq!(line.send(&raw, false), (Vec::new(), raw.to_vec()));
        assert_eq!(line.receive(&raw, Instant::now()), (raw.to_vec(), None));
        line.set_binary(false);
        assert_eq!(line.send(b"\r", true).1, b"\r\n");
    }

    #[test]
    fn auto_login_replies_are_mapped_and_encoded() {
        let password = SecretString::new("密码1");
        let mut line = Discipline::new(
            &RawConfig {
                encoding: Encoding::Gb18030,
                enter: Enter::Crlf,
                local_echo: LocalEcho::Off,
                username: "admin".into(),
                password: Some(password),
                ..Default::default()
            },
            Instant::now(),
        );
        let (out, reply) = line.receive(b"Username:", Instant::now());
        assert_eq!(out, b"Username:");
        assert_eq!(&reply.unwrap()[..], b"admin\r\n");
        let (_, reply) = line.receive(b"\r\nPassword: ", Instant::now());
        assert_eq!(&reply.unwrap()[..], b"\xc3\xdc\xc2\xeb1\r\n");
        // 传输文件期间不检查提示
        let mut line = Discipline::new(
            &RawConfig {
                username: "admin".into(),
                ..Default::default()
            },
            Instant::now(),
        );
        line.set_binary(true);
        assert_eq!(line.receive(b"login:", Instant::now()).1, None);
    }
}
