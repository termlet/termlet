//! 启动参数解析的测试（含 .xsh 的编码与密码解码）。

use super::*;

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// 没有会话文件时的解析。
fn launch(list: &[&str]) -> Option<LaunchTarget> {
    parse_with(&strings(list), |_| None)
}

/// 一个 SSH 终端的请求，其余字段用结构更新语法覆盖。
fn ssh(address: &str, port: u16, username: &str) -> LaunchTarget {
    LaunchTarget {
        kind: LaunchKind::Terminal,
        address: address.into(),
        port,
        username: username.into(),
        password: None,
        name: None,
        encoding: None,
        source: LaunchSource::Url,
    }
}

fn secret(value: &str) -> Option<SecretString> {
    Some(SecretString::new(value))
}

fn exposed(target: &Option<LaunchTarget>) -> Option<&str> {
    target.as_ref()?.password.as_ref().map(SecretString::expose)
}

/// Xshell 的会话文件是带 BOM 的 UTF-16LE。
fn utf16(text: &str) -> Vec<u8> {
    let mut out = vec![0xFF, 0xFE];
    out.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    out
}

fn base64(text: &[u8]) -> String {
    data_encoding::BASE64.encode(text)
}

/// 只有地址与用户名的会话文件，后面接上 `extra`（放在 [CONNECTION:AUTHENTICATION] 节中）。
fn xsh_with(extra: &str) -> Vec<u8> {
    format!("[CONNECTION]\nHost=h\n[CONNECTION:AUTHENTICATION]\nUserName=u\n{extra}\n").into_bytes()
}

// ---- parseUrl ----

#[test]
fn url_decodes_userinfo_and_splits_at_last_at() {
    assert_eq!(
        parse_url("ssh://JMS-1f2e:a%40b%3Ac@jms.example.com:2222"),
        Some(LaunchTarget {
            password: secret("a@b:c"),
            ..ssh("jms.example.com", 2222, "JMS-1f2e")
        })
    );
    assert_eq!(
        parse_url("sftp://root@[::1]/home/root"),
        Some(LaunchTarget {
            kind: LaunchKind::Sftp,
            ..ssh("::1", 22, "root")
        })
    );
    let t = parse_url("ssh://user@my@host").unwrap();
    assert_eq!(
        (t.username.as_str(), t.address.as_str()),
        ("user@my", "host")
    );
    // 路径中的 @ 不算
    let t = Some(parse_url("sftp://ops:pw@10.0.0.5/home/a@b/").unwrap());
    let u = t.as_ref().unwrap();
    assert_eq!(
        (u.username.as_str(), u.address.as_str()),
        ("ops", "10.0.0.5")
    );
    assert_eq!(exposed(&t), Some("pw"));
    // 密码中有没编码的 / 时（地址部分没有 @）仍以最后一个 @ 为界
    let t = Some(parse_url("ssh://u:ab/cd@h:2222").unwrap());
    assert_eq!(t.as_ref().unwrap().address, "h");
    assert_eq!(exposed(&t), Some("ab/cd"));
}

#[test]
fn url_rejects_unrecognized() {
    for raw in [
        "http://a@b",
        "ssh://host-only",
        "ssh://u@h:70000",
        "ssh://u@h:0",
        "ssh://u@h:",
        "ssh://u@h:22x",
        "ssh://u@",
        "ssh://@h",
        "ssh://:p@h",
        "ssh://u@[::1]x",
        "ssh://u@h\nx",
        "ssh:/u@h",
        "xssh://u@h",
        // sftp:// 的用户名中不能有 /（地址部分没有 @，按最后一个 @ 为界后用户名是 a/b）
        "sftp://a/b@h",
        "scp://a%2Fb@h",
        // 最后一个 @ 之后没有地址
        "ssh://u@h/x@",
        "ssh://u@h/x@:22",
    ] {
        assert_eq!(parse_url(raw), None, "{raw}");
    }
}

#[test]
fn ssh_url_splits_at_the_last_at_like_xshell() {
    let parts = |raw: &str| {
        let t = parse_url(raw).unwrap();
        let password = t.password.as_ref().map(|p| p.expose().to_string());
        (t.username, password, t.address, t.port)
    };
    let owned = |user: &str, password: Option<&str>, address: &str, port: u16| {
        (
            user.to_string(),
            password.map(str::to_string),
            address.to_string(),
            port,
        )
    };
    // JumpServer 一类堡垒机：用户名是“用户@资产”，其中有 @ 与 /（ShellRS issue #32 的例子）
    assert_eq!(
        parts("ssh://11xx@shine.com/资产名:密码@opsall.shine.com:8013"),
        owned(
            "11xx@shine.com/资产名",
            Some("密码"),
            "opsall.shine.com",
            8013
        )
    );
    // 用户名中有 /
    assert_eq!(parts("ssh://a/b@h"), owned("a/b", None, "h", 22));
    // 用户名中有 @（不编码）；最后一个 @ 之后的路径照旧忽略
    assert_eq!(
        parts("ssh://root@10.0.0.8@jump.example.com:2222/x?y#z"),
        owned("root@10.0.0.8", None, "jump.example.com", 2222)
    );
    // 用户名与密码以第一个 : 为界，密码中可以有 : 与 @
    assert_eq!(
        parts("ssh://u@asset:p:w@d@h"),
        owned("u@asset", Some("p:w@d"), "h", 22)
    );
    // IPv6
    assert_eq!(
        parts("ssh://admin@web/x:tok@[2001:db8::1]:2200"),
        owned("admin@web/x", Some("tok"), "2001:db8::1", 2200)
    );
    assert_eq!(parts("ssh://u@[::1]"), owned("u", None, "::1", 22));
    // 编码的写法解码后照用
    assert_eq!(
        parts("ssh://a%40b%2Fc:p%3Aq@h"),
        owned("a@b/c", Some("p:q"), "h", 22)
    );
}

#[test]
fn sftp_url_keeps_the_winscp_rule() {
    // 路径中的 @ 不算：地址部分中最后一个 @ 为界
    let t = parse_url("sftp://ops@jump:pw@10.0.0.5:2222/home/a@b/").unwrap();
    assert_eq!(
        (t.username.as_str(), t.address.as_str(), t.port),
        ("ops@jump", "10.0.0.5", 2222)
    );
    assert_eq!(t.password.as_ref().map(SecretString::expose), Some("pw"));
    let t = parse_url("scp://u@[::1]:2200/srv/x@y").unwrap();
    assert_eq!(
        (t.kind, t.username.as_str(), t.address.as_str(), t.port),
        (LaunchKind::Sftp, "u", "::1", 2200)
    );
    // 同一个链接按 ssh:// 解析时以整个链接中最后一个 @ 为界（Xshell 的链接没有路径）
    let t = parse_url("ssh://u@h/home/a@b/").unwrap();
    assert_eq!(
        (t.username.as_str(), t.address.as_str()),
        ("u@h/home/a", "b")
    );
}

#[test]
fn url_details() {
    // scp 按 SFTP；协议不分大小写；前后的空白忽略
    assert_eq!(parse_url("scp://u@h").unwrap().kind, LaunchKind::Sftp);
    assert_eq!(parse_url(" SSH://u@h:2200 ").unwrap().port, 2200);
    // 查询参数、片段与路径一样忽略
    assert_eq!(parse_url("ssh://u@h:2222?x=1").unwrap().port, 2222);
    assert_eq!(parse_url("sftp://u@h#frag").unwrap().address, "h");
    assert_eq!(
        parse_url("ssh://u@[fe80::1]:2200/x").unwrap().address,
        "fe80::1"
    );
    // 密码为空按没有密码；+ 不是空格
    assert_eq!(parse_url("ssh://u:@h").unwrap().password, None);
    let t = Some(parse_url("ssh://a+b:c+d@h").unwrap());
    assert_eq!(t.as_ref().unwrap().username, "a+b");
    assert_eq!(exposed(&t), Some("c+d"));
    // 解码失败（不是两位十六进制数、不是 UTF-8）时原样保留
    let t = Some(parse_url("ssh://a%zz:p%4@h").unwrap());
    assert_eq!(t.as_ref().unwrap().username, "a%zz");
    assert_eq!(exposed(&t), Some("p%4"));
    assert_eq!(parse_url("ssh://u%ff@h").unwrap().username, "u%ff");
    assert_eq!(
        parse_url("ssh://%E7%94%A8%E6%88%B7@h").unwrap().username,
        "用户"
    );
    // 密码中的 / 与 ? 不影响地址（地址从最后一个 @ 之后开始）
    let t = Some(parse_url("ssh://u:a/b?c@h:2222/x").unwrap());
    assert_eq!(t.as_ref().unwrap().port, 2222);
    assert_eq!(exposed(&t), Some("a/b?c"));
}

// ---- parseLaunchArgs ----

#[test]
fn xshell_newtab_and_url_ignoring_double_dash_switches() {
    let args = [
        "--allow-file-access-from-files",
        "-newtab",
        "root@web01[2026-10-12_10_00_00]",
        "-url",
        "ssh://JMS-uuid:Tok3n@jms.x.com:2222",
    ];
    assert_eq!(
        launch(&args),
        Some(LaunchTarget {
            password: secret("Tok3n"),
            name: Some("root@web01[2026-10-12_10_00_00]".into()),
            source: LaunchSource::Xshell,
            ..ssh("jms.x.com", 2222, "JMS-uuid")
        })
    );
    // -url 认不出时不再试别的格式
    assert_eq!(launch(&["-url", "ssh://host-only", "ssh://u@h"]), None);
}

#[test]
fn osms_xsh_utf16_with_base64_password_and_user_info() {
    let xsh = [
        "[CONNECTION]",
        "Host=10.1.1.1",
        "Port=60022",
        "Protocol=SSH",
        "[CONNECTION:AUTHENTICATION]",
        "UserName=admin#10.2.2.2#root",
        &format!("Password={}", base64(b"p@ss w0rd")),
        "Method=2",
        "[TERMINAL]",
        "CodePage=936",
    ]
    .join("\r\n");
    let data = utf16(&xsh);
    let path = r"C:\Temp\s\test.xsh";
    let t = parse_with(&strings(&["-newtab", "测试会话", path]), |file| {
        (file == path).then(|| data.clone())
    });
    assert_eq!(
        t,
        Some(LaunchTarget {
            password: secret("p@ss w0rd"),
            name: Some("测试会话".into()),
            encoding: Some(Encoding::Gb18030),
            source: LaunchSource::Xsh,
            ..ssh("10.1.1.1", 60022, "admin#10.2.2.2#root")
        })
    );
    // Xshell 自己加密保存的密码：解不出可读的文本，不使用（连接时询问）
    assert_eq!(
        parse_xsh(&xsh_with("Password=AAECAwQF")),
        Some(LaunchTarget {
            source: LaunchSource::Xsh,
            ..ssh("h", 22, "u")
        })
    );
    assert_eq!(launch(&["missing.xsh"]), None);
}

#[test]
fn putty_ssh_user_at_host_port_and_password() {
    assert_eq!(
        launch(&[".", "-ssh", "JMS-u@jms.x.com", "-P", "2222", "-pw", "Tok3n"]),
        Some(LaunchTarget {
            password: secret("Tok3n"),
            source: LaunchSource::Putty,
            ..ssh("jms.x.com", 2222, "JMS-u")
        })
    );
}

#[test]
fn securecrt_flags_and_address() {
    let args: Vec<&str> =
        "/N web01 /T /SSH2 /ACCEPTHOSTKEYS /p 2222 /password Tok3n /L JMS-u jms.x.com"
            .split(' ')
            .collect();
    assert_eq!(
        launch(&args),
        Some(LaunchTarget {
            password: secret("Tok3n"),
            name: Some("web01".into()),
            source: LaunchSource::SecureCrt,
            ..ssh("jms.x.com", 2222, "JMS-u")
        })
    );
}

#[test]
fn mobaxterm_newtab_with_sshpass_command() {
    let command = "sshpass -pTok3n ssh -o StrictHostKeyChecking=no JMS-u@jms.x.com -p2222";
    assert_eq!(
        launch(&["-newtab", command]),
        Some(LaunchTarget {
            password: secret("Tok3n"),
            source: LaunchSource::MobaXterm,
            ..ssh("jms.x.com", 2222, "JMS-u")
        })
    );
}

#[test]
fn winscp_xftp_links_and_session_name() {
    assert_eq!(
        launch(&["sftp://JMS-u:Tok3n@jms.x.com:2222/", "/sessionname=web01"]),
        Some(LaunchTarget {
            kind: LaunchKind::Sftp,
            password: secret("Tok3n"),
            name: Some("web01".into()),
            source: LaunchSource::WinScp,
            ..ssh("jms.x.com", 2222, "JMS-u")
        })
    );
    let t = launch(&["-newwin", "sftp://u@h"]).unwrap();
    assert_eq!((t.kind, t.source), (LaunchKind::Sftp, LaunchSource::WinScp));
    let t = launch(&["ssh://u@h:2200"]).unwrap();
    assert_eq!((t.port, t.source), (2200, LaunchSource::Url));
}

#[test]
fn plain_start_is_none() {
    assert_eq!(launch(&[]), None);
    assert_eq!(launch(&[".", "--inspect"]), None);
    assert_eq!(launch(&["--", "--url=ssh://u@h"]), None);
}

// ---- 补充：参数的写法 ----

#[test]
fn flags_are_case_insensitive_and_in_any_order() {
    let t = launch(&["-URL", "ssh://u:p@h", "-NewTab", "web"]).unwrap();
    assert_eq!(
        (t.source, t.name.as_deref()),
        (LaunchSource::Xshell, Some("web"))
    );
    let t = launch(&["H", "/l", "u", "/SSH2", "/P", "2200"]).unwrap();
    assert_eq!(
        (t.source, t.address.as_str(), t.port),
        (LaunchSource::SecureCrt, "H", 2200)
    );
    let t = launch(&["-PW", "x", "u@h", "-SSH"]).unwrap();
    assert_eq!(t.source, LaunchSource::Putty);
    assert_eq!(exposed(&Some(t)), Some("x"));
    let t = launch(&["-SessionName=\"web 01\"", "-NEWWIN", "SSH://u@h"]).unwrap();
    assert_eq!(
        (t.kind, t.source, t.name.as_deref()),
        (LaunchKind::Terminal, LaunchSource::WinScp, Some("web 01"))
    );
}

#[test]
fn newtab_value_as_name() {
    // 像开关、链接或会话文件时不是名称
    let t = launch(&["-newtab", "-url", "ssh://u@h"]).unwrap();
    assert_eq!((t.source, t.name), (LaunchSource::Xshell, None));
    let t = launch(&["-newtab", "ssh://u@h"]).unwrap();
    assert_eq!((t.source, t.name), (LaunchSource::Url, None));
    // 裸链接没有 /sessionname= 时用 -newtab 的值；名称去掉引号与空白
    let t = launch(&["-newtab", " \"web 01\" ", "ssh://u@h"]).unwrap();
    assert_eq!(t.name.as_deref(), Some("web 01"));
    let t = launch(&["-newtab", "tab", "/sessionname=", "sftp://u@h"]).unwrap();
    assert_eq!(t.name.as_deref(), Some("tab"));
    let t = launch(&["-newtab", "tab", "/sessionname=sess", "sftp://u@h"]).unwrap();
    assert_eq!(t.name.as_deref(), Some("sess"));
    // 第一个链接
    let t = launch(&["sftp://a@x", "ssh://b@y"]).unwrap();
    assert_eq!((t.username.as_str(), t.kind), ("a", LaunchKind::Sftp));
}

#[test]
fn mobaxterm_variants() {
    // 认不出 sshpass 命令时不再试别的格式
    assert_eq!(
        launch(&["-newtab", "sshpass -pX ssh nohost", "ssh://u@h"]),
        None
    );
    assert_eq!(launch(&["-newtab", "sshpass -pX ssh @h"]), None);
    assert_eq!(launch(&["-newtab", "sshpass -pX ssh u@h -p"]), None);
    assert_eq!(launch(&["-newtab", "sshpass -pX ssh u@h -p 0"]), None);
    // 引号中的密码、分开写的 -p，跳过 -o、-l、-i 的值；没有端口时为 22
    let t = launch(&[
        "-newtab",
        "  sshpass -p \"a b\" ssh -l x -i k@f -p 2200 u@h",
    ]);
    assert_eq!(exposed(&t), Some("a b"));
    let t = t.unwrap();
    assert_eq!(
        (t.username.as_str(), t.address.as_str(), t.port),
        ("u", "h", 2200)
    );
    let t = launch(&["-newtab", "sshpass ssh JMS-1@corp@h"]).unwrap();
    assert_eq!(
        (t.username.as_str(), t.port, t.password),
        ("JMS-1@corp", 22, None)
    );
}

#[test]
fn putty_variants() {
    // -l 给出用户；-load、-i 的值跳过
    let t = launch(&[
        "-load", "sess", "-ssh", "h", "-l", "admin", "-i", "k@f.ppk", "-P", "2200",
    ]);
    assert_eq!(
        t,
        Some(LaunchTarget {
            source: LaunchSource::Putty,
            ..ssh("h", 2200, "admin")
        })
    );
    // “用户@地址”优先于 -l
    assert_eq!(launch(&["-ssh", "-l", "x", "u@h"]).unwrap().username, "u");
    // 小写的 -p 不是端口（跳过它的值）
    assert_eq!(launch(&["-ssh", "u@h", "-p", "2200"]).unwrap().port, 22);
    // 没有用户、没有地址、-P 后面没有值或不是端口
    for args in [
        &["-ssh", "h"][..],
        &["-ssh", "u@"],
        &["-ssh", "u@h", "-P"],
        &["-ssh", "u@h", "-P", "x"],
    ] {
        assert_eq!(launch(args), None, "{args:?}");
    }
}

#[test]
fn securecrt_variants() {
    let t = launch(&[
        "/SSH2",
        "/N",
        "\"web 01\"",
        "/i",
        "key",
        "/arg",
        "x",
        "/L",
        "u",
        "h",
    ])
    .unwrap();
    assert_eq!(
        (t.address.as_str(), t.port, t.name.as_deref()),
        ("h", 22, Some("web 01"))
    );
    // /P 是最后一个参数时按 22
    assert_eq!(launch(&["/SSH2", "/L", "u", "h", "/P"]).unwrap().port, 22);
    assert_eq!(launch(&["/SSH2", "/L", "u", "h", "/P", "0"]), None);
    assert_eq!(launch(&["/SSH2", "h"]), None);
    assert_eq!(launch(&["/SSH2", "/L", "u"]), None);
}

// ---- 补充：.xsh 会话文件 ----

#[test]
fn xsh_file_from_caller_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("test.xsh"),
        utf16("[CONNECTION]\r\nHost=10.0.0.5\r\n[CONNECTION:AUTHENTICATION]\r\nUserName=root\r\n"),
    )
    .unwrap();
    // 相对路径按调起者的当前目录
    let t = parse(&strings(&["-newtab", "会话", "test.xsh"]), dir.path()).unwrap();
    assert_eq!(
        (t.address.as_str(), t.name.as_deref(), t.source),
        ("10.0.0.5", Some("会话"), LaunchSource::Xsh)
    );
    // 绝对路径；-newtab 的值本身是会话文件时没有名称
    let full = dir.path().join("test.xsh");
    let t = parse(
        &strings(&["-newtab", full.to_str().unwrap()]),
        Path::new("."),
    )
    .unwrap();
    assert_eq!((t.address.as_str(), t.name), ("10.0.0.5", None));
    assert_eq!(parse(&strings(&["other.xsh"]), dir.path()), None);

    // 超过 64KB 的不是会话文件
    let mut big = b"[CONNECTION]\nHost=h\n[CONNECTION:AUTHENTICATION]\nUserName=u\n".to_vec();
    big.resize(MAX_XSH_SIZE as usize + 1, b'\n');
    std::fs::write(dir.path().join("big.xsh"), &big).unwrap();
    assert_eq!(parse(&strings(&["big.xsh"]), dir.path()), None);
    big.truncate(MAX_XSH_SIZE as usize);
    std::fs::write(dir.path().join("big.xsh"), &big).unwrap();
    assert!(parse(&strings(&["big.xsh"]), dir.path()).is_some());
    // 扩展名不分大小写
    let data = std::fs::read(&full).unwrap();
    let t = parse_with(&strings(&["S.XSH"]), |file| {
        (file == "S.XSH").then_some(data)
    });
    assert_eq!(t.map(|t| t.address), Some("10.0.0.5".into()));
}

#[test]
fn xsh_text_encodings() {
    let text =
        "[CONNECTION]\r\nHost=10.0.0.5\r\n[CONNECTION:AUTHENTICATION]\r\nUserName=管理员\r\n";
    let user = |data: &[u8]| parse_xsh(data).map(|t| t.username);
    // UTF-16LE（带 BOM，Xshell）
    assert_eq!(user(&utf16(text)), Some("管理员".into()));
    // UTF-8，带与不带 BOM
    assert_eq!(user(text.as_bytes()), Some("管理员".into()));
    let mut bom = vec![0xEF, 0xBB, 0xBF];
    bom.extend_from_slice(text.as_bytes());
    assert_eq!(user(&bom), Some("管理员".into()));
    // 不是 UTF-8 时按 GBK
    let (gbk, _, _) = encoding_rs::GBK.encode(text);
    assert!(std::str::from_utf8(&gbk).is_err());
    assert_eq!(user(&gbk), Some("管理员".into()));
}

#[test]
fn xsh_fields() {
    let target = |text: &str| parse_xsh(text.as_bytes());
    // 节名、键名不分大小写，值前后的空白忽略，用户名原样
    let t = target("[connection]\nHOST = h \nport= 2200\nprotocol=ssh\n[Connection:Authentication]\nusername= a#b#c \n").unwrap();
    assert_eq!(
        t,
        LaunchTarget {
            source: LaunchSource::Xsh,
            ..ssh("h", 2200, "a#b#c")
        }
    );
    // 没写协议按 SSH，没写端口按 22；第一个节之前的行忽略
    assert_eq!(
        target("Host=x\n[CONNECTION]\nHost=h\nProtocol=\n[CONNECTION:AUTHENTICATION]\nUserName=u")
            .map(|t| (t.address, t.port)),
        Some(("h".into(), 22))
    );
    for text in [
        "[CONNECTION]\nHost=h\nProtocol=TELNET\n[CONNECTION:AUTHENTICATION]\nUserName=u",
        "[CONNECTION]\nHost=h\nPort=0\n[CONNECTION:AUTHENTICATION]\nUserName=u",
        "[CONNECTION]\nHost=h\nPort=abc\n[CONNECTION:AUTHENTICATION]\nUserName=u",
        "[CONNECTION]\n[CONNECTION:AUTHENTICATION]\nUserName=u",
        "[CONNECTION]\nHost=h\n[CONNECTION:AUTHENTICATION]\nUserName=",
        "[CONNECTION]\nHost=h\nUserName=u",
        "",
    ] {
        assert_eq!(target(text), None, "{text}");
    }
    // CodePage
    let encoding = |page: &str| {
        let text = format!(
            "[CONNECTION]\nHost=h\n[CONNECTION:AUTHENTICATION]\nUserName=u\n[TERMINAL]\nCodePage={page}"
        );
        target(&text).unwrap().encoding
    };
    assert_eq!(encoding("936"), Some(Encoding::Gb18030));
    assert_eq!(encoding("54936"), Some(Encoding::Gb18030));
    assert_eq!(encoding(" 950 "), Some(Encoding::Big5));
    assert_eq!(encoding("65001"), Some(Encoding::Utf8));
    assert_eq!(encoding("1252"), None);
}

#[test]
fn xsh_password_decoding() {
    let password = |value: &str| {
        let t = parse_xsh(&xsh_with(&format!("Password={value}")));
        exposed(&t).map(str::to_string)
    };
    assert_eq!(
        password(&base64("密码 123".as_bytes())),
        Some("密码 123".into())
    );
    assert_eq!(
        password(&format!(" {} ", base64(b"pass"))),
        Some("pass".into())
    );
    // 与 atob 相同，最后一个字符中多余的位不检查
    assert_eq!(password("QR=="), Some("A".into()));
    for bad in [
        // 控制字符（Xshell 加密后的数据）
        base64(b"\x01abc"),
        base64(b"ab\x7f"),
        // 不是 UTF-8
        base64(&[0xFF, 0xFE, 0x41]),
        // 不是 Base64、长度不对、等号不在末尾或太多
        "abc!".into(),
        "abcde".into(),
        "ab=c".into(),
        "A===".into(),
        "====".into(),
        String::new(),
    ] {
        assert_eq!(password(&bad), None, "{bad}");
    }
}

// ---- isLaunchTarget ----

#[test]
fn validity_of_targets() {
    let ok = ssh("h", 22, "u");
    assert!(ok.is_valid());
    assert!(
        LaunchTarget {
            password: secret("p"),
            name: Some("n".into()),
            encoding: Some(Encoding::Gb18030),
            ..ok.clone()
        }
        .is_valid()
    );
    let long = |n: usize| "x".repeat(n);
    assert!(
        LaunchTarget {
            address: long(255),
            username: long(255),
            password: secret(&long(4096)),
            name: Some("名".repeat(4096)),
            ..ok.clone()
        }
        .is_valid()
    );
    for bad in [
        LaunchTarget {
            address: String::new(),
            ..ok.clone()
        },
        LaunchTarget {
            address: long(256),
            ..ok.clone()
        },
        LaunchTarget {
            port: 0,
            ..ok.clone()
        },
        LaunchTarget {
            username: long(256),
            ..ok.clone()
        },
        LaunchTarget {
            password: secret(&long(4097)),
            ..ok.clone()
        },
        LaunchTarget {
            name: Some(long(4097)),
            ..ok.clone()
        },
    ] {
        assert!(!bad.is_valid(), "{bad:?}");
    }
    // 解析结果不合法时不返回
    assert_eq!(launch(&[format!("ssh://u@{}", long(256)).as_str()]), None);
    assert_eq!(launch(&["-newtab", "sshpass ssh u@"]), None);
    assert_eq!(launch(&["-newtab", long(4097).as_str(), "ssh://u@h"]), None);
}

// ---- 显示、日志与临时连接（adhoc） ----

#[test]
fn debug_and_log_summary_hide_password() {
    let t = launch(&["-newtab", "web", "-url", "ssh://JMS-u:Tok3n@[::1]:2222"]).unwrap();
    let debug = format!("{t:?}");
    assert!(!debug.contains("Tok3n") && debug.contains("***"), "{debug}");
    assert_eq!(
        t.log_summary(),
        r#"source=xshell kind=terminal address="::1" port=2222"#
    );
}

#[test]
fn title_and_display_target() {
    let t = ssh("10.0.0.1", 2222, "root");
    assert_eq!(t.title(), "root@10.0.0.1");
    assert_eq!(t.display_target(), "root@10.0.0.1:2222");
    let t = LaunchTarget {
        name: Some("web01".into()),
        ..ssh("::1", 22, "u")
    };
    assert_eq!(t.title(), "web01");
    assert_eq!(t.display_target(), "u@[::1]:22");
    let t = LaunchTarget {
        name: Some(String::new()),
        ..ssh("::1", 22, "u")
    };
    assert_eq!(t.title(), "u@::1");
}

#[test]
fn launched_connections_use_single_channel() {
    // 临时连接总是只用一个通道
    let t = launch(&["ssh://u@h:2200"]).unwrap();
    assert!(t.single_channel());
    assert_eq!(
        t.quick_target(),
        QuickTarget {
            username: "u".into(),
            host: "h".into(),
            port: 2200,
        }
    );
}
