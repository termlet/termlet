// 关键词高亮预设的语料：每条预设的正例与反例、默认规则与全部预设下的整行结果。期望先在 Python 中按与引擎相同的
// 匹配方式（规则按顺序、互不重叠）模拟过，这里用真实的引擎再跑一遍。

use termlet_core::model::{HIGHLIGHT_PRESETS, HighlightPreset, HighlightSettings};
use termlet_terminal::Highlighter;

use crate::terminal_colors::{core_color, rule_spec};

fn highlighter(rules: &[termlet_core::model::HighlightRule]) -> Highlighter {
    let specs: Vec<_> = rules.iter().map(rule_spec).collect();
    let (highlighter, errors) = Highlighter::new(&specs);
    assert!(errors.is_empty(), "{errors:?}");
    highlighter
}

fn preset(id: &str) -> &'static HighlightPreset {
    HIGHLIGHT_PRESETS
        .iter()
        .find(|p| p.id == id)
        .unwrap_or_else(|| panic!("没有预设 {id}"))
}

/// 每条预设单独匹配：(预设, 文字, 期望标出的部分)。
const CASES: &[(&str, &str, &[&str])] = &[
    (
        "ipv6",
        "2001:0db8:85a3:0000:0000:8a2e:0370:7334",
        &["2001:0db8:85a3:0000:0000:8a2e:0370:7334"],
    ),
    ("ipv6", "2001:db8:1:2:3:4:5:6", &["2001:db8:1:2:3:4:5:6"]),
    (
        "ipv6",
        "2001:db8:85a3::8a2e:370:7334",
        &["2001:db8:85a3::8a2e:370:7334"],
    ),
    ("ipv6", "2001:DB8::1", &["2001:DB8::1"]),
    ("ipv6", "::1", &["::1"]),
    ("ipv6", "ping6 ::1", &["::1"]),
    ("ipv6", "fe80::1", &["fe80::1"]),
    ("ipv6", "ff02::1", &["ff02::1"]),
    ("ipv6", "fe80::", &["fe80::"]),
    ("ipv6", "fe80::/64 dev eth0", &["fe80::/64"]),
    ("ipv6", "2001:db8::/32", &["2001:db8::/32"]),
    (
        "ipv6",
        "inet6 fe80::a00:27ff:fe4e:66a1/64 scope link",
        &["fe80::a00:27ff:fe4e:66a1/64"],
    ),
    ("ipv6", "inet6 ::1/128 scope host", &["::1/128"]),
    ("ipv6", "fe80::1%eth0", &["fe80::1%eth0"]),
    (
        "ipv6",
        "fe80::1c2d:3e4f:5a6b:7c8d%12",
        &["fe80::1c2d:3e4f:5a6b:7c8d%12"],
    ),
    ("ipv6", "fe80::1%eth0.100 up", &["fe80::1%eth0.100"]),
    ("ipv6", "fe80::1%eth0.", &["fe80::1%eth0"]),
    ("ipv6", "::ffff:192.0.2.1", &["::ffff:192.0.2.1"]),
    ("ipv6", "::ffff:0:192.0.2.1", &["::ffff:0:192.0.2.1"]),
    ("ipv6", "64:ff9b::192.0.2.33", &["64:ff9b::192.0.2.33"]),
    (
        "ipv6",
        "0:0:0:0:0:ffff:192.0.2.1",
        &["0:0:0:0:0:ffff:192.0.2.1"],
    ),
    ("ipv6", "[::1]:22", &["[::1]:22"]),
    ("ipv6", "[::]:22", &["[::]:22"]),
    ("ipv6", "[::]:*", &["[::]"]),
    ("ipv6", "[2001:db8::1]:8080", &["[2001:db8::1]:8080"]),
    ("ipv6", "[fe80::1%12]:49668", &["[fe80::1%12]:49668"]),
    (
        "ipv6",
        "[::ffff:127.0.0.1]:8080",
        &["[::ffff:127.0.0.1]:8080"],
    ),
    (
        "ipv6",
        "http://[2001:db8::1]:8080/path",
        &["[2001:db8::1]:8080"],
    ),
    ("ipv6", "ssh root@2001:db8::1", &["2001:db8::1"]),
    (
        "ipv6",
        "64 bytes from 2001:db8::1: icmp_seq=1",
        &["2001:db8::1"],
    ),
    ("ipv6", "tcp6  0  0 :::22   :::*   LISTEN", &[":::22"]),
    ("ipv6", "tcp6  0  0 ::1:631   :::*   LISTEN", &["::1:631"]),
    ("ipv6", "ACCEPT all ::/0 ::/0", &["::/0", "::/0"]),
    ("ipv6", "1::", &["1::"]),
    ("ipv6", "1:2:3:4:5:6:7::", &["1:2:3:4:5:6:7::"]),
    ("ipv6", "::2:3:4:5:6:7:8", &["::2:3:4:5:6:7:8"]),
    ("ipv6", "dead:beef::1", &["dead:beef::1"]),
    ("ipv6", "2001:db8::dead:beef", &["2001:db8::dead:beef"]),
    ("ipv6", "地址2001:db8::1，", &["2001:db8::1"]),
    ("ipv6", "from=2001:db8::1,port", &["2001:db8::1"]),
    ("ipv6", "12:34:56", &[]),
    ("ipv6", "12:34:56.789", &[]),
    ("ipv6", "2026-11-20 12:00:00", &[]),
    ("ipv6", "aa:bb:cc:dd:ee:ff", &[]),
    ("ipv6", "AA:BB:CC:DD:EE:FF", &[]),
    ("ipv6", "std::vector<int>", &[]),
    ("ipv6", "crate::foo::bar", &[]),
    ("ipv6", "Foo::bar", &[]),
    ("ipv6", "Self::add", &[]),
    ("ipv6", "a::b", &[]),
    ("ipv6", "abc::def", &[]),
    ("ipv6", "::", &[]),
    ("ipv6", "x :: Int", &[]),
    (
        "ipv6",
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        &[],
    ),
    ("ipv6", "commit 3f2a9cd1", &[]),
    (
        "ipv6",
        "MD5:16:27:ac:a5:76:28:2d:36:63:1b:56:4d:eb:df:a6:48",
        &[],
    ),
    (
        "ipv6",
        "SHA1 Fingerprint=AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01",
        &[],
    ),
    (
        "ipv6",
        "    00:c3:5b:2a:91:7d:4e:aa:01:02:03:04:05:06:07:",
        &[],
    ),
    ("ipv6", "[20/Nov/2026:12:00:00 +0800]", &[]),
    ("ipv6", "1:2.34-5ubuntu1", &[]),
    ("ipv6", "10.0.0.12:5432", &[]),
    (
        "ipv6",
        "eth0: flags=4163<UP,BROADCAST,RUNNING,MULTICAST>  mtu 1500",
        &[],
    ),
    ("ipv6", "00:11:22:33:44:55:66:77", &[]),
    ("ipv6", "::face", &[]),
    ("ipv6", "Foo::face", &[]),
    ("ipv6", "std::abs", &[]),
    (
        "mac",
        "link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff",
        &["52:54:00:12:34:56", "ff:ff:ff:ff:ff:ff"],
    ),
    (
        "mac",
        "Physical Address. . . : 00-1A-2B-3C-4D-5E",
        &["00-1A-2B-3C-4D-5E"],
    ),
    ("mac", "0050.56c0.0001  DYNAMIC  Gi0/1", &["0050.56c0.0001"]),
    (
        "mac",
        "ether AA:BB:CC:DD:EE:FF txqueuelen",
        &["AA:BB:CC:DD:EE:FF"],
    ),
    ("mac", "12:34:56", &[]),
    ("mac", "2001:db8::1", &[]),
    ("mac", "550e8400-e29b-41d4-a716-446655440000", &[]),
    ("mac", "aa:bb:cc:dd:ee", &[]),
    ("mac", "aa:bb-cc:dd:ee:ff", &[]),
    ("mac", "2026-11-20", &[]),
    (
        "mac",
        "MD5:16:27:ac:a5:76:28:2d:36:63:1b:56:4d:eb:df:a6:48",
        &["16:27:ac:a5:76:28:2d:36:63:1b:56:4d:eb:df:a6:48"],
    ),
    (
        "mac",
        "00:11:22:33:44:55:66:77",
        &["00:11:22:33:44:55:66:77"],
    ),
    (
        "mac",
        "        00:c3:5b:2a:91:7d:4e:aa:01:02:03:04:05:06:07:",
        &["00:c3:5b:2a:91:7d:4e:aa:01:02:03:04:05:06:07"],
    ),
    (
        "url",
        "see https://termlet.dev/docs.",
        &["https://termlet.dev/docs"],
    ),
    (
        "url",
        "(https://example.com/a?b=1&c=2)",
        &["https://example.com/a?b=1&c=2"],
    ),
    (
        "url",
        "访问 https://example.com/x，谢谢",
        &["https://example.com/x"],
    ),
    (
        "url",
        "git clone git@github.com:a/b.git 或 ssh://git@host:2222/repo.git",
        &["ssh://git@host:2222/repo.git"],
    ),
    (
        "url",
        "redis://:pass@10.0.0.1:6379/0",
        &["redis://:pass@10.0.0.1:6379/0"],
    ),
    (
        "url",
        "jdbc:mysql://db:3306/app?useSSL=false",
        &["mysql://db:3306/app?useSSL=false"],
    ),
    ("url", "http://[::1]:8080/", &["http://[::1]:8080/"]),
    ("url", r#""https://a.com/x""#, &["https://a.com/x"]),
    ("url", "HTTPS://A.COM", &["HTTPS://A.COM"]),
    ("url", r#"C:\Windows\System32"#, &[]),
    ("url", "a://", &[]),
    ("url", "http:/x", &[]),
    ("url", "time 12:00:00", &[]),
    (
        "datetime",
        "2026-11-20T12:00:00Z",
        &["2026-11-20T12:00:00Z"],
    ),
    (
        "datetime",
        "2026-11-20T12:00:00.123+08:00",
        &["2026-11-20T12:00:00.123+08:00"],
    ),
    (
        "datetime",
        "2026-11-20T12:00:00+0800",
        &["2026-11-20T12:00:00+0800"],
    ),
    (
        "datetime",
        "2026-11-20 12:00:00 INFO",
        &["2026-11-20 12:00:00"],
    ),
    (
        "datetime",
        "2026-11-20 12:00:00.123 INFO",
        &["2026-11-20 12:00:00.123"],
    ),
    (
        "datetime",
        "2026-11-20 12:00:00,123 INFO",
        &["2026-11-20 12:00:00,123"],
    ),
    (
        "datetime",
        "2026/11/20 12:00:00 [error]",
        &["2026/11/20 12:00:00"],
    ),
    ("datetime", "2026.11.20 12:00", &["2026.11.20 12:00"]),
    ("datetime", "date: 2026-11-20", &["2026-11-20"]),
    (
        "datetime",
        "Nov 20 12:00:00 web-01 sshd[123]:",
        &["Nov 20 12:00:00"],
    ),
    ("datetime", "Nov  5 01:02:03 web-01", &["Nov  5 01:02:03"]),
    (
        "datetime",
        "Thu Nov 20 12:00:00 CST 2026",
        &["Thu Nov 20 12:00:00"],
    ),
    (
        "datetime",
        "-rw-r--r-- 1 root root 0 Nov 20 12:00 a.txt",
        &["Nov 20 12:00"],
    ),
    (
        "datetime",
        "-rw-r--r-- 1 root root 0 Nov 20  2025 a.txt",
        &["Nov 20  2025"],
    ),
    ("datetime", "Nov 20, 2026", &["Nov 20, 2026"]),
    (
        "datetime",
        r#"10.0.0.1 - - [20/Nov/2026:12:00:00 +0800] "GET /""#,
        &["20/Nov/2026:12:00:00 +0800"],
    ),
    (
        "datetime",
        "date: Thu, 20 Nov 2026 04:00:00 GMT",
        &["Thu, 20 Nov 2026 04:00:00 GMT"],
    ),
    ("datetime", "[12:00:01] started", &["12:00:01"]),
    ("datetime", "12:00:01.123 started", &["12:00:01.123"]),
    (
        "datetime",
        "2026年11月20日 星期四 12:00:00 CST",
        &["2026年11月20日", "12:00:00"],
    ),
    ("datetime", "2026年 11月 20日", &["2026年 11月 20日"]),
    (
        "datetime",
        "root  1  0.0  0.1 ... 00:01:23 /sbin/init",
        &["00:01:23"],
    ),
    ("datetime", "10.0.0.12:5432", &[]),
    ("datetime", "version 1.2.3", &[]),
    ("datetime", "1234-5678-9012", &[]),
    ("datetime", "2026-13-45x", &[]),
    ("datetime", "listen 8080", &[]),
    ("datetime", "12:00 noon", &[]),
    ("datetime", "may be 20 or 12:00", &[]),
    (
        "uuid",
        "request_id=550e8400-e29b-41d4-a716-446655440000 done",
        &["550e8400-e29b-41d4-a716-446655440000"],
    ),
    (
        "uuid",
        "{6F9619FF-8B86-D011-B42D-00C04FC964FF}",
        &["6F9619FF-8B86-D011-B42D-00C04FC964FF"],
    ),
    ("uuid", "550e8400e29b41d4a716446655440000", &[]),
    ("uuid", "550e8400-e29b-41d4-a716-4466554400001", &[]),
    ("number", "retry 1/3 after 30 ms", &["1", "3", "30"]),
    (
        "number",
        "took 230ms, size 1.5GiB, 3.14",
        &["230ms", "1.5GiB", "3.14"],
    ),
    ("number", "version 1.2.3", &["1.2.3"]),
    ("number", "addr 0x7ffd5e3c", &["0x7ffd5e3c"]),
    ("number", "eth0 x86_64 sha256 md5 v2 abc123", &[]),
    ("number", "50% done", &["50"]),
    ("number", "-5 degrees", &["5"]),
    ("number", "3f2a9cd1", &[]),
    ("number", "node-01", &["01"]),
];

#[test]
fn presets_match_the_corpus() {
    let mut wrong = Vec::new();
    for (id, text, want) in CASES {
        let highlighter = highlighter(&[preset(id).rule()]);
        let got: Vec<&str> = highlighter
            .find_matches(text)
            .into_iter()
            .map(|m| &text[m.range])
            .collect();
        if got != *want {
            wrong.push(format!("{id} {text:?}：标出 {got:?}，应为 {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// 整行：标出的部分与颜色。
type Line = (&'static str, &'static [(&'static str, &'static str)]);

/// 新装时的默认规则。
const DEFAULT_LINES: &[Line] = &[
    (
        "12:00:01 ERROR connect 10.0.0.12:5432 refused · 12:00:02 WARN retry 1/3 via 2001:db8::1 · 12:00:03 OK connected",
        &[
            ("ERROR", "red"),
            ("10.0.0.12", "cyan"),
            ("refused", "red"),
            ("WARN", "yellow"),
            ("2001:db8::1", "cyan"),
            ("OK", "green"),
        ],
    ),
    (
        "2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP group default qlen 1000",
        &[],
    ),
    (
        "    link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff",
        &[],
    ),
    (
        "    inet 10.0.2.15/24 brd 10.0.2.255 scope global dynamic eth0",
        &[("10.0.2.15/24", "cyan"), ("10.0.2.255", "cyan")],
    ),
    (
        "    inet6 fe80::5054:ff:fe12:3456/64 scope link",
        &[("fe80::5054:ff:fe12:3456/64", "cyan")],
    ),
    (
        r#"LISTEN 0      128          [::]:22            [::]:*    users:(("sshd",pid=812,fd=4))"#,
        &[("[::]:22", "cyan"), ("[::]", "cyan")],
    ),
    (
        "LISTEN 0      4096   [::ffff:127.0.0.1]:8080          *:*",
        &[("[::ffff:127.0.0.1]:8080", "cyan")],
    ),
    (
        "tcp6       0      0 :::22                   :::*                    LISTEN      812/sshd",
        &[(":::22", "cyan")],
    ),
    (
        "   Link-local IPv6 Address . . . . . : fe80::1c2d:3e4f:5a6b:7c8d%12",
        &[("fe80::1c2d:3e4f:5a6b:7c8d%12", "cyan")],
    ),
    (
        "   Physical Address. . . . . . . . . : 00-1A-2B-3C-4D-5E",
        &[],
    ),
    (
        r#"2001:db8::1 - - [20/Nov/2026:12:00:00 +0800] "GET /api/v1/hosts/42 HTTP/1.1" 200 512 "-" "curl/8.5.0""#,
        &[("2001:db8::1", "cyan")],
    ),
    (
        "Nov 20 12:00:00 web-01 sshd[1234]: Accepted publickey for root from 2001:db8::5 port 52314 ssh2",
        &[("2001:db8::5", "cyan")],
    ),
    (
        "2026-11-20 12:00:01.123 ERROR [550e8400-e29b-41d4-a716-446655440000] connect ::ffff:10.0.0.12 failed after 30 ms",
        &[
            ("ERROR", "red"),
            ("::ffff:10.0.0.12", "cyan"),
            ("failed", "red"),
        ],
    ),
    (
        "SHA1 Fingerprint=AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01",
        &[],
    ),
    (
        "error[E0599]: no function or associated item named `add` found for struct `Foo` in the current scope: Foo::add",
        &[("error", "red")],
    ),
    ("        00:c3:5b:2a:91:7d:4e:aa:01:02:03:04:05:06:07:", &[]),
    (
        "see https://termlet.dev/docs/guide#highlight for 2026-11-20 release",
        &[],
    ),
];

/// 全部预设（按优先级）。
const ALL_LINES: &[Line] = &[
    (
        "2026-11-20 12:00:02 WARN 配置项已弃用 deprecated，下载 https://termlet.dev/download 完成，大小 12.5MiB，耗时 230ms",
        &[
            ("2026-11-20 12:00:02", "blue"),
            ("WARN", "yellow"),
            ("deprecated", "yellow"),
            ("https://termlet.dev/download", "blue"),
            ("12.5MiB", "magenta"),
            ("230ms", "magenta"),
        ],
    ),
    (
        "12:00:01 ERROR connect 10.0.0.12:5432 refused · 12:00:02 WARN retry 1/3 via 2001:db8::1 · 12:00:03 OK connected",
        &[
            ("12:00:01", "blue"),
            ("ERROR", "red"),
            ("10.0.0.12", "cyan"),
            ("5432", "magenta"),
            ("refused", "red"),
            ("12:00:02", "blue"),
            ("WARN", "yellow"),
            ("1", "magenta"),
            ("3", "magenta"),
            ("2001:db8::1", "cyan"),
            ("12:00:03", "blue"),
            ("OK", "green"),
        ],
    ),
    (
        "2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500 qdisc fq_codel state UP group default qlen 1000",
        &[("2", "magenta"), ("1500", "magenta"), ("1000", "magenta")],
    ),
    (
        "    link/ether 52:54:00:12:34:56 brd ff:ff:ff:ff:ff:ff",
        &[("52:54:00:12:34:56", "cyan"), ("ff:ff:ff:ff:ff:ff", "cyan")],
    ),
    (
        "    inet 10.0.2.15/24 brd 10.0.2.255 scope global dynamic eth0",
        &[("10.0.2.15/24", "cyan"), ("10.0.2.255", "cyan")],
    ),
    (
        "    inet6 fe80::5054:ff:fe12:3456/64 scope link",
        &[("fe80::5054:ff:fe12:3456/64", "cyan")],
    ),
    (
        r#"LISTEN 0      128          [::]:22            [::]:*    users:(("sshd",pid=812,fd=4))"#,
        &[
            ("0", "magenta"),
            ("128", "magenta"),
            ("[::]:22", "cyan"),
            ("[::]", "cyan"),
            ("812", "magenta"),
            ("4", "magenta"),
        ],
    ),
    (
        "LISTEN 0      4096   [::ffff:127.0.0.1]:8080          *:*",
        &[
            ("0", "magenta"),
            ("4096", "magenta"),
            ("[::ffff:127.0.0.1]:8080", "cyan"),
        ],
    ),
    (
        "tcp6       0      0 :::22                   :::*                    LISTEN      812/sshd",
        &[
            ("0", "magenta"),
            ("0", "magenta"),
            (":::22", "cyan"),
            ("812", "magenta"),
        ],
    ),
    (
        "   Link-local IPv6 Address . . . . . : fe80::1c2d:3e4f:5a6b:7c8d%12",
        &[("fe80::1c2d:3e4f:5a6b:7c8d%12", "cyan")],
    ),
    (
        "   Physical Address. . . . . . . . . : 00-1A-2B-3C-4D-5E",
        &[("00-1A-2B-3C-4D-5E", "cyan")],
    ),
    (
        r#"2001:db8::1 - - [20/Nov/2026:12:00:00 +0800] "GET /api/v1/hosts/42 HTTP/1.1" 200 512 "-" "curl/8.5.0""#,
        &[
            ("2001:db8::1", "cyan"),
            ("20/Nov/2026:12:00:00 +0800", "blue"),
            ("42", "magenta"),
            ("1.1", "magenta"),
            ("200", "magenta"),
            ("512", "magenta"),
            ("8.5.0", "magenta"),
        ],
    ),
    (
        "Nov 20 12:00:00 web-01 sshd[1234]: Accepted publickey for root from 2001:db8::5 port 52314 ssh2",
        &[
            ("Nov 20 12:00:00", "blue"),
            ("01", "magenta"),
            ("1234", "magenta"),
            ("2001:db8::5", "cyan"),
            ("52314", "magenta"),
        ],
    ),
    (
        "2026-11-20 12:00:01.123 ERROR [550e8400-e29b-41d4-a716-446655440000] connect ::ffff:10.0.0.12 failed after 30 ms",
        &[
            ("2026-11-20 12:00:01.123", "blue"),
            ("ERROR", "red"),
            ("550e8400-e29b-41d4-a716-446655440000", "magenta"),
            ("::ffff:10.0.0.12", "cyan"),
            ("failed", "red"),
            ("30", "magenta"),
        ],
    ),
    (
        "SHA1 Fingerprint=AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01",
        &[(
            "AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01",
            "cyan",
        )],
    ),
    (
        "error[E0599]: no function or associated item named `add` found for struct `Foo` in the current scope: Foo::add",
        &[("error", "red")],
    ),
    (
        "        00:c3:5b:2a:91:7d:4e:aa:01:02:03:04:05:06:07:",
        &[("00:c3:5b:2a:91:7d:4e:aa:01:02:03:04:05:06:07", "cyan")],
    ),
    (
        "see https://termlet.dev/docs/guide#highlight for 2026-11-20 release",
        &[
            ("https://termlet.dev/docs/guide#highlight", "blue"),
            ("2026-11-20", "blue"),
        ],
    ),
    (
        "redirect to https://example.com/login?error=access_denied&from=10.0.0.1 failed",
        &[
            (
                "https://example.com/login?error=access_denied&from=10.0.0.1",
                "blue",
            ),
            ("failed", "red"),
        ],
    ),
    (
        "GET http://10.0.0.12:8080/api/ok 200",
        &[("http://10.0.0.12:8080/api/ok", "blue"), ("200", "magenta")],
    ),
    (
        "open http://[2001:db8::1]:8080/status",
        &[("http://[2001:db8::1]:8080/status", "blue")],
    ),
];

fn check_lines(highlighter: &Highlighter, lines: &[Line]) -> Vec<String> {
    let mut wrong = Vec::new();
    for (text, want) in lines {
        let got: Vec<(&str, String)> = highlighter
            .find_matches(text)
            .into_iter()
            .map(|m| (&text[m.range], core_color(m.color).name().into_owned()))
            .collect();
        let want: Vec<(&str, String)> = want.iter().map(|(t, c)| (*t, c.to_string())).collect();
        if got != want {
            wrong.push(format!("{text:?}：标出 {got:?}，应为 {want:?}"));
        }
    }
    wrong
}

#[test]
fn whole_lines_follow_the_preset_order() {
    let defaults = highlighter(&HighlightSettings::default().rules);
    let all: Vec<_> = HIGHLIGHT_PRESETS
        .iter()
        .map(HighlightPreset::rule)
        .collect();
    let all = highlighter(&all);
    assert!(all.is_merged(), "全部预设应能合并成一个 RegexSet");
    let mut wrong = check_lines(&defaults, DEFAULT_LINES);
    wrong.extend(check_lines(&all, ALL_LINES));
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

/// 匹配的耗时：`cargo test -p termlet-desktop --release highlight_presets_speed -- --ignored --nocapture`
#[test]
#[ignore = "耗时测量，手动运行"]
fn highlight_presets_speed() {
    use std::time::Instant;
    // 典型的日志与命令输出，一半含中文
    let samples = [
        "2026-11-20 12:00:01.123 ERROR [550e8400-e29b-41d4-a716-446655440000] connect 10.0.0.12:5432 refused after 30 ms",
        "Nov 20 12:00:00 web-01 sshd[1234]: Accepted publickey for root from 2001:db8::5 port 52314 ssh2",
        "LISTEN 0      128          [::]:22            [::]:*    users:((\"sshd\",pid=812,fd=4))",
        "    inet6 fe80::5054:ff:fe12:3456/64 scope link",
        "10.0.0.1 - - [20/Nov/2026:12:00:00 +0800] \"GET /api/v1/hosts/42 HTTP/1.1\" 200 512 \"-\" \"curl/8.5.0\"",
        "2026-11-20 12:00:02 WARN 配置项已弃用 deprecated，请改用新的写法 request_id=0123456789abcdef",
        "2026-11-20 12:00:03 INFO 服务启动完成，监听 [::]:8080，耗时 230ms",
        "连接 10.0.0.12 失败：connection refused，3 秒后重试（第 1/3 次）",
        "用户 root 从 2001:db8::1 登录成功 ok",
        "下载 https://termlet.dev/download/termlet-0.3.3-x64.msi 完成，大小 12.5MiB",
    ];
    let lines: Vec<String> = (0..10_000)
        .map(|i| format!("{} #{i}", samples[i % samples.len()]))
        .collect();
    let legacy: Vec<_> = HighlightSettings::default()
        .rules
        .into_iter()
        .filter(|r| r.pattern != preset("ipv6").pattern)
        .collect();
    let all: Vec<_> = HIGHLIGHT_PRESETS
        .iter()
        .map(HighlightPreset::rule)
        .collect();
    for (name, rules) in [
        ("0.3.2 的 4 条", legacy),
        ("默认 5 条", HighlightSettings::default().rules),
        ("全部 10 条", all),
    ] {
        let highlighter = highlighter(&rules);
        for (label, ascii) in [("纯 ASCII", true), ("含中文", false)] {
            let picked: Vec<&String> = lines.iter().filter(|l| l.is_ascii() == ascii).collect();
            let started = Instant::now();
            let mut found = 0;
            for line in &picked {
                found += std::hint::black_box(highlighter.find_matches(line)).len();
            }
            println!(
                "{name}，{label}：每行 {:.2} µs（{} 行，{found} 处）",
                started.elapsed().as_secs_f64() * 1e6 / picked.len() as f64,
                picked.len()
            );
        }
    }
}
