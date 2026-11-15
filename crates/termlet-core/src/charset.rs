//! 终端的编码转换：远端是 GBK / GB18030、Big5 时，输出解码成 UTF-8 交给终端，输入从 UTF-8 编码回去。
//!
//! 解码是流式的：一个多字节字符被拆在两次读取之间时，前半部分留到下一次。
//! 转义序列都是 ASCII，在这些编码中保持不变，先解码再交给终端解析不会破坏它们。

use encoding_rs::{BIG5, CoderResult, Decoder, Encoder, GB18030};

use crate::model::Encoding;

/// 一个方向的流式解码器（远端 → UTF-8）。UTF-8 时原样通过。
pub struct OutputDecoder {
    decoder: Option<Decoder>,
}

impl OutputDecoder {
    pub fn new(encoding: Encoding) -> Self {
        let decoder = match encoding {
            Encoding::Utf8 => None,
            Encoding::Gb18030 => Some(GB18030.new_decoder_without_bom_handling()),
            Encoding::Big5 => Some(BIG5.new_decoder_without_bom_handling()),
        };
        Self { decoder }
    }

    /// 解码一段输出，返回 UTF-8 字节。
    pub fn decode(&mut self, input: &[u8]) -> Vec<u8> {
        let Some(decoder) = self.decoder.as_mut() else {
            return input.to_vec();
        };
        let capacity = decoder
            .max_utf8_buffer_length(input.len())
            .unwrap_or(input.len() * 3 + 16);
        let mut output = String::with_capacity(capacity);
        let (result, _, _) = decoder.decode_to_string(input, &mut output, false);
        debug_assert_eq!(result, CoderResult::InputEmpty);
        output.into_bytes()
    }
}

/// 输入方向的编码器（UTF-8 → 远端）。
pub struct InputEncoder {
    encoder: Option<Encoder>,
}

impl InputEncoder {
    pub fn new(encoding: Encoding) -> Self {
        let encoder = match encoding {
            Encoding::Utf8 => None,
            Encoding::Gb18030 => Some(GB18030.new_encoder()),
            Encoding::Big5 => Some(BIG5.new_encoder()),
        };
        Self { encoder }
    }

    /// 编码一段输入。`input` 是终端送来的 UTF-8；不完整的 UTF-8（不应出现）按替换字符处理。
    pub fn encode(&mut self, input: &[u8]) -> Vec<u8> {
        let Some(encoder) = self.encoder.as_mut() else {
            return input.to_vec();
        };
        let text = String::from_utf8_lossy(input);
        let capacity = encoder
            .max_buffer_length_from_utf8_if_no_unmappables(text.len())
            .unwrap_or(text.len() * 4 + 16)
            + 16;
        let mut output = Vec::with_capacity(capacity);
        // Big5 中没有的字符编码成 HTML 数字字符引用（encoding_rs 的行为），远端会看到 &#…;
        // 引用比预估的长：它只写到 Vec 剩余的容量为止（不扩容），写满了扩容后接着编码
        let mut rest: &str = &text;
        loop {
            let (result, read, _) = encoder.encode_from_utf8_to_vec(rest, &mut output, false);
            rest = &rest[read..];
            match result {
                CoderResult::InputEmpty => break,
                CoderResult::OutputFull => output.reserve(rest.len() * 4 + 32),
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_passes_through() {
        let mut decoder = OutputDecoder::new(Encoding::Utf8);
        assert_eq!(decoder.decode("你好".as_bytes()), "你好".as_bytes());
    }

    #[test]
    fn gbk_split_across_reads() {
        // “中文” 的 GBK 编码：D6 D0 CE C4
        let mut decoder = OutputDecoder::new(Encoding::Gb18030);
        let mut out = decoder.decode(&[b'a', 0xD6]);
        out.extend(decoder.decode(&[0xD0, 0xCE, 0xC4, b'\x1b', b'[', b'm']));
        assert_eq!(String::from_utf8(out).unwrap(), "a中文\x1b[m");
    }

    #[test]
    fn encode_input_round_trip() {
        let mut encoder = InputEncoder::new(Encoding::Gb18030);
        assert_eq!(
            encoder.encode("ls 中文\r".as_bytes()),
            b"ls \xD6\xD0\xCE\xC4\r"
        );
        let mut big5 = InputEncoder::new(Encoding::Big5);
        let mut decoder = OutputDecoder::new(Encoding::Big5);
        let bytes = big5.encode("繁體".as_bytes());
        assert_eq!(String::from_utf8(decoder.decode(&bytes)).unwrap(), "繁體");
    }

    #[test]
    fn big5_unmappable_input_is_not_truncated() {
        // Big5 没有的字符写成 &#NNNNN;，比预估的长：一个也不能少
        let input = "😀".repeat(40) + "简体字结尾";
        let mut big5 = InputEncoder::new(Encoding::Big5);
        let mut decoder = OutputDecoder::new(Encoding::Big5);
        let text = String::from_utf8(decoder.decode(&big5.encode(input.as_bytes()))).unwrap();
        assert_eq!(text.matches("&#128512;").count(), 40, "{text}");
        assert!(text.ends_with('尾'), "{text}");
    }
}
