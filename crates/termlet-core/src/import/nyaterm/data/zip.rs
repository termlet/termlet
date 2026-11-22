//! `.nya` 解开后是一个 zip（`manifest.json` 与 `snapshot.redb`，deflate）。这里只读其中一项：按中央目录找到它，
//! 只认 stored 与 deflate，不支持 zip64（NyaTerm 的快照上限 50 MiB），解压后的大小与 CRC-32 都要对得上。

use std::io::Read as _;

use flate2::Crc;
use flate2::read::DeflateDecoder;
use zeroize::Zeroizing;

const LOCAL_HEADER: u32 = 0x0403_4b50;
const CENTRAL_HEADER: u32 = 0x0201_4b50;
const END_OF_CENTRAL: u32 = 0x0605_4b50;
const STORED: u16 = 0;
const DEFLATED: u16 = 8;

/// 是否是 zip（以本地文件头开始）。
pub(super) fn is_zip(data: &[u8]) -> bool {
    data.starts_with(&LOCAL_HEADER.to_le_bytes())
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// 解压名为 `name` 的项；没有这一项、格式不对或解压后超过 `limit` 字节时为 `None`。
pub(super) fn extract(data: &[u8], name: &str, limit: usize) -> Option<Zeroizing<Vec<u8>>> {
    // 中央目录结束记录在最后（之后最多 65535 字节的注释）
    let tail_start = data.len().saturating_sub(22 + 0xFFFF);
    let end = (tail_start..=data.len().checked_sub(22)?)
        .rev()
        .find(|&at| u32_at(data, at) == Some(END_OF_CENTRAL))?;
    let entries = usize::from(u16_at(data, end + 10)?);
    let mut at = usize::try_from(u32_at(data, end + 16)?).ok()?;
    for _ in 0..entries {
        if u32_at(data, at)? != CENTRAL_HEADER {
            return None;
        }
        let method = u16_at(data, at + 10)?;
        let crc = u32_at(data, at + 16)?;
        let compressed = usize::try_from(u32_at(data, at + 20)?).ok()?;
        let size = usize::try_from(u32_at(data, at + 24)?).ok()?;
        let name_len = usize::from(u16_at(data, at + 28)?);
        let extra_len = usize::from(u16_at(data, at + 30)?);
        let comment_len = usize::from(u16_at(data, at + 32)?);
        let local = usize::try_from(u32_at(data, at + 42)?).ok()?;
        let entry_name = data.get(at + 46..at + 46 + name_len)?;
        at += 46 + name_len + extra_len + comment_len;
        if entry_name != name.as_bytes() {
            continue;
        }
        if size > limit || u32_at(data, local)? != LOCAL_HEADER {
            return None;
        }
        let start = local
            + 30
            + usize::from(u16_at(data, local + 26)?)
            + usize::from(u16_at(data, local + 28)?);
        let body = data.get(start..start.checked_add(compressed)?)?;
        let out = match method {
            STORED => Zeroizing::new(body.to_vec()),
            DEFLATED => {
                let mut out = Zeroizing::new(Vec::with_capacity(size));
                DeflateDecoder::new(body)
                    .take(limit as u64 + 1)
                    .read_to_end(&mut out)
                    .ok()?;
                out
            }
            _ => return None,
        };
        let mut check = Crc::new();
        check.update(&out);
        return (out.len() == size && check.sum() == crc).then_some(out);
    }
    None
}

/// 测试用：写一个只有 `entries` 的 zip（deflate），与 NyaTerm 的 zip 库写出的结构相同（本地头中有大小与 CRC）。
#[cfg(test)]
pub(super) fn write(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;

    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, content) in entries {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut crc = Crc::new();
        crc.update(content);
        let offset = out.len() as u32;
        let fields = |sig: u32, central: bool| {
            let mut h = sig.to_le_bytes().to_vec();
            if central {
                h.extend(20u16.to_le_bytes()); // 创建者版本
            }
            h.extend(20u16.to_le_bytes()); // 需要的版本
            h.extend(0u16.to_le_bytes()); // 标志
            h.extend(DEFLATED.to_le_bytes());
            h.extend(0u32.to_le_bytes()); // 时间、日期
            h.extend(crc.sum().to_le_bytes());
            h.extend((compressed.len() as u32).to_le_bytes());
            h.extend((content.len() as u32).to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes()); // 扩展字段长度
            if central {
                h.extend([0u8; 6]); // 注释长度、磁盘号、内部属性
                h.extend(0u32.to_le_bytes()); // 外部属性
                h.extend(offset.to_le_bytes());
            }
            h.extend(name.as_bytes());
            h
        };
        out.extend(fields(LOCAL_HEADER, false));
        out.extend(&compressed);
        central.extend(fields(CENTRAL_HEADER, true));
    }
    let central_offset = out.len() as u32;
    out.extend(&central);
    out.extend(END_OF_CENTRAL.to_le_bytes());
    out.extend([0u8; 4]); // 磁盘号
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((entries.len() as u16).to_le_bytes());
    out.extend((central.len() as u32).to_le_bytes());
    out.extend(central_offset.to_le_bytes());
    out.extend(0u16.to_le_bytes()); // 注释长度
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_one_entry() {
        let payload: Vec<u8> = (0..5000u32).flat_map(|i| i.to_le_bytes()).collect();
        let zip = write(&[
            (
                "manifest.json",
                br#"{"format":"nyaterm-portable-snapshot-zip"}"#,
            ),
            ("snapshot.redb", &payload),
        ]);
        assert!(is_zip(&zip));
        assert_eq!(
            &extract(&zip, "snapshot.redb", 1 << 20).unwrap()[..],
            &payload[..]
        );
        assert!(extract(&zip, "missing", 1 << 20).is_none());
        assert!(extract(&zip, "snapshot.redb", 100).is_none(), "超过上限");
        let mut broken = zip.clone();
        let at = 30 + "manifest.json".len() + 5;
        broken[at] ^= 0xFF;
        assert!(
            extract(&broken, "manifest.json", 1 << 20).is_none(),
            "CRC 不对"
        );
        assert!(extract(b"not a zip", "snapshot.redb", 1 << 20).is_none());
    }
}
