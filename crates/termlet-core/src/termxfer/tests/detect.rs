//! 检测传输的起始序列。

use super::super::detect::Detector;
use super::super::info::Protocol;
use super::super::zmodem::crc16;

/// lrzsz 实际输出的帧头：sz 的 ZRQINIT 与 rz 的 ZRINIT（参数 0x23，CRC be50）。
pub(super) const ZRQINIT_HEADER: &[u8] = b"**\x18B00000000000000\r\x8a\x11";
pub(super) const ZRINIT_HEADER: &[u8] = b"**\x18B0100000023be50\r\x8a\x11";

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn zmodem_detect() {
    assert_eq!(crc16(&[1, 0, 0, 0, 0x23]), 0xbe50, "CRC-16/XMODEM 计算不对");
    let mut d = Detector::new();
    let fed = d.feed(cat(&[b"$ sz a.txt\r\nrz\r", ZRQINIT_HEADER]));
    let trig = fed.trigger.expect("sz 应触发下载");
    assert_eq!(trig.protocol, Protocol::Zmodem);
    assert!(!trig.upload);
    assert_eq!(fed.show, b"$ sz a.txt\r\nrz\r");
    assert_eq!(fed.rest, b"\r\x8a\x11");

    let fed = Detector::new().feed(cat(&[b"rz waiting to receive.", ZRINIT_HEADER]));
    assert!(fed.trigger.expect("rz 应触发上传").upload);
    assert_eq!(fed.show, b"rz waiting to receive.");

    // CRC 不对、类型不是 ZRQINIT / ZRINIT、不是十六进制：不触发
    for s in [
        &b"**\x18B0100000023be51"[..],
        b"**\x18B0400000000000000",
        b"**\x18B01xyz00023be50",
    ] {
        let fed = Detector::new().feed(cat(&[b"x", s, b"\r\n"]));
        assert!(fed.trigger.is_none(), "{s:?} 不应触发");
    }
}

#[test]
fn zmodem_detect_split() {
    let mut d = Detector::new();
    // 帧头被拆到两次读取中：前一段的末尾先暂扣
    let fed = d.feed(b"prompt$ **\x18B0100".to_vec());
    assert!(fed.trigger.is_none());
    assert_eq!(fed.show, b"prompt$ ");
    assert!(d.holding(), "帧头的开头应暂扣");
    let fed = d.feed(b"000023be50\r\x8a\x11".to_vec());
    assert!(fed.trigger.expect("拼起来后应触发").upload);
    assert!(fed.show.is_empty());
    assert_eq!(fed.rest, b"\r\x8a\x11");

    // 结尾的 * 暂扣，之后不是帧头时原样显示
    let mut d = Detector::new();
    let fed = d.feed(b"a*".to_vec());
    assert_eq!(fed.show, b"a");
    assert!(d.holding(), "结尾的 * 应暂扣");
    let fed = d.feed(b"b".to_vec());
    assert!(fed.trigger.is_none());
    assert_eq!(fed.show, b"*b", "不是帧头时原样显示");
    // 等不到后续时由 flush 取回
    d.feed(b"**".to_vec());
    assert_eq!(d.flush(), b"**");
    assert!(!d.holding());
    // 不像帧头的不暂扣
    let fed = Detector::new().feed(b"x**\x18B05".to_vec());
    assert_eq!(
        fed.show, b"x**\x18B05",
        "不可能是 ZRQINIT / ZRINIT 的不暂扣"
    );
}

#[test]
fn zmodem_detect_suppressed() {
    // 中止后的一段时间内不再检测 ZModem（远端的 sz 可能重发起始帧），trzsz 照常
    let mut d = Detector::new();
    d.suppress_zmodem(std::time::Duration::from_secs(60));
    let fed = d.feed(cat(&[b"x", ZRQINIT_HEADER]));
    assert!(fed.trigger.is_none());
    assert_eq!(fed.show, cat(&[b"x", ZRQINIT_HEADER]));
    let fed = d.feed(b"::TRZSZ:TRANSFER:S:1.2.0:1728631234567900\r\n".to_vec());
    assert!(fed.trigger.is_some());
}

#[test]
fn trzsz_detect() {
    let mut d = Detector::new();
    let out = b"\x1b7\x07::TRZSZ:TRANSFER:S:1.2.0:1728631234567100\r\n";
    let fed = d.feed(cat(&[b"$ tsz a.txt\r\n", out, b"#CFG"]));
    let trig = fed.trigger.expect("tsz 应触发下载");
    assert_eq!(trig.protocol, Protocol::Trzsz);
    assert!(!trig.upload);
    assert_eq!(trig.version, "1.2.0");
    assert_eq!(trig.unique_id, "1728631234567100");
    assert!(!trig.win_server);
    let show = String::from_utf8_lossy(&fed.show);
    assert!(
        show.contains("::TRZSZGO:TRANSFER:S"),
        "触发行原样显示（TRZSZ 改为 TRZSZGO）：{show:?}"
    );
    assert_eq!(fed.rest, b"#CFG", "其后属于传输");
    // 同一唯一 ID 再次出现（重绘、回显）：不再触发
    assert!(
        d.feed(out.to_vec()).trigger.is_none(),
        "重复的唯一 ID 不应再触发"
    );

    let trig = Detector::new()
        .feed(b"::TRZSZ:TRANSFER:R:1.2.0:1728631234567200\r\n".to_vec())
        .trigger
        .expect("trz 应触发上传");
    assert!(trig.upload && !trig.directory);
    let trig = Detector::new()
        .feed(b"::TRZSZ:TRANSFER:D:1.2.0:1728631234567300\n".to_vec())
        .trigger
        .expect("trz -d 应触发上传目录");
    assert!(trig.upload && trig.directory);
    // 唯一 ID 以 10 结尾：远端是 Windows
    let trig = Detector::new()
        .feed(b"::TRZSZ:TRANSFER:R:1.2.0:1728631234510\r\n".to_vec())
        .trigger
        .unwrap();
    assert!(trig.win_server, "应识别 Windows 服务端");

    // 触发行还没结束：暂扣，收完后触发
    let mut d = Detector::new();
    let fed = d.feed(b"$ ::TRZSZ:TRANSFER:S:1.2.0:17286".to_vec());
    assert!(fed.trigger.is_none());
    assert_eq!(fed.show, b"$ ", "触发行未结束时应暂扣");
    let trig = d
        .feed(b"31234567400\r\n".to_vec())
        .trigger
        .expect("收完后应触发");
    assert_eq!(trig.unique_id, "1728631234567400");

    // 以前传输的输出（例如 cat 日志）：后面带有 Saved 等字样，不触发
    let log = b"::TRZSZ:TRANSFER:S:1.2.0:1728631234567500\r\n...............Saved 1 file\r\n";
    assert!(
        Detector::new().feed(log.to_vec()).trigger.is_none(),
        "日志中的触发行不应触发"
    );
}

// 经过 tmux 时触发行后面可能没有换行：CI 上 trz 一直等不到回答的那次，tmux 输出的原样（复位滚动区域、定位光标）
#[test]
fn trzsz_trigger_redrawn_by_tmux() {
    let mut d = Detector::new();
    let chunk = b"\x1b[10;39r\x1b[30S\x1b[9;1H\x1b[K::TRZSZ:TRANSFER:R:1.2.0:9427688042920:41063\x1b[1;40r\x1b[10;1H";
    let fed = d.feed(chunk.to_vec());
    let trig = fed.trigger.expect("触发行后面是控制序列时也应识别");
    assert!(trig.upload && !trig.directory);
    assert_eq!(trig.unique_id, "9427688042920");
    assert!(!trig.win_server);
    let show = String::from_utf8_lossy(&fed.show);
    assert!(
        show.ends_with("::TRZSZGO:TRANSFER:R:1.2.0:9427688042920:41063\x1b[1;40r\x1b[10;1H"),
        "触发行与其后的控制序列一并显示：{show:?}"
    );
    assert!(fed.rest.is_empty(), "{:?}", fed.rest);
    // 控制序列之后的属于传输；不完整的控制序列也交给传输（读行时跳过）
    let fed = Detector::new()
        .feed(b"::TRZSZ:TRANSFER:S:1.2.0:9427688042900:0\x1b[1;40r\r\nnext\x1b[1;4".to_vec());
    assert!(fed.trigger.is_some());
    assert_eq!(fed.rest, b"next\x1b[1;4");
    let fed = Detector::new().feed(b"::TRZSZ:TRANSFER:S:1.2.0:9427688042910\x1b[1".to_vec());
    assert!(fed.trigger.expect("应识别").win_server);
    assert_eq!(fed.rest, b"\x1b[1");

    // 结尾是触发行（端口、唯一 ID 可能没收完）：暂扣；等不到后续时（settle）按完整的触发行开始传输
    for tail in [&b"94276880"[..], b"9427688042920:", b"9427688042920:41063"] {
        let mut d = Detector::new();
        let fed = d.feed(cat(&[b"$ ::TRZSZ:TRANSFER:R:1.2.0:", tail]));
        assert!(fed.trigger.is_none(), "{tail:?}");
        assert_eq!(fed.show, b"$ ");
        assert!(d.holding());
        let fed = d.settle();
        let trig = fed.trigger.expect("等不到后续时应开始传输");
        assert!(trig.upload);
        assert!(String::from_utf8_lossy(&fed.show).starts_with("::TRZSZGO:"));
        assert!(!d.holding());
    }
    // 收完后照常识别（端口在后一段中）
    let mut d = Detector::new();
    assert!(
        d.feed(b"::TRZSZ:TRANSFER:R:1.2.0:9427688042920:".to_vec())
            .trigger
            .is_none()
    );
    let trig = d.feed(b"41063\r\n".to_vec()).trigger.expect("收完后应触发");
    assert_eq!(trig.unique_id, "9427688042920");
    // 暂扣的不是触发行：settle 照常显示
    let mut d = Detector::new();
    d.feed(b"abc::TRZ".to_vec());
    let fed = d.settle();
    assert!(fed.trigger.is_none());
    assert_eq!(fed.show, b"::TRZ");
}

#[test]
fn plain_output_passes_through_unchanged() {
    // 普通输出（含中文、转义序列、二进制）原样通过，不暂扣
    let mut d = Detector::new();
    let text = "你好 \x1b[31mred\x1b[0m\r\n".as_bytes().to_vec();
    let fed = d.feed(text.clone());
    assert_eq!(fed.show, text);
    assert!(!d.holding());
    let binary = super::noise(64 * 1024, 7);
    let fed = d.feed(binary.clone());
    assert!(fed.trigger.is_none());
    let mut shown = fed.show;
    shown.extend(d.flush());
    assert_eq!(shown, binary);
}
