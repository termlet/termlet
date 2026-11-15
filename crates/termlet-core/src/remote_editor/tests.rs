use std::path::PathBuf;

use super::detect::{command_program, has_app_bundle, has_scheme_handler, mac_bundles};
use super::ssh_config::{Appended, append};
use super::*;
use crate::ssh::plan::{Hop, ProxyPlan};

const STAMP: &str = "2026-11-09 18:00";

fn endpoint() -> Endpoint {
    Endpoint {
        name: "Web 生产".into(),
        address: "10.0.0.5".into(),
        port: 22,
        user: "alice".into(),
        identity_file: None,
        certificate_file: None,
        jumps: Vec::new(),
        proxy: None,
        stored_key: false,
    }
}

fn ssh(endpoint: Endpoint) -> Remote {
    Remote::Ssh(endpoint)
}

fn launch(editor: Editor, remote: &Remote, path: &str, config: &str) -> Launch {
    plan(editor, remote, path, &SshConfig::parse(config), "me", STAMP).expect("能打开")
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

// 链接：VS Code 的 ssh-remote（小写的 user@host 原样，含大写或端口的写成十六进制 JSON），Zed 的 zed://ssh
// （带非 22 的端口），路径中的空格、中文、# 等转义
#[test]
fn links_without_alias() {
    let remote = ssh(endpoint());
    let code = launch(Editor::VsCode, &remote, "/srv/www", "");
    assert_eq!(
        code.url,
        "vscode://vscode-remote/ssh-remote+alice@10.0.0.5/srv/www"
    );
    assert_eq!(code.host, "alice@10.0.0.5");
    assert_eq!(code.alias, None);
    assert!(code.needs_confirm(), "没有别名时先问");

    let zed = launch(Editor::Zed, &remote, "/srv/my site/#1", "");
    assert_eq!(zed.url, "zed://ssh/alice@10.0.0.5/srv/my%20site/%231");

    let remote = ssh(Endpoint {
        address: "Web.Example.com".into(),
        port: 2222,
        user: "Bob".into(),
        ..endpoint()
    });
    let code = launch(Editor::VsCode, &remote, "/home/Bob/项目", "");
    let authority = hex(r#"{"hostName":"Bob@Web.Example.com:2222"}"#);
    assert_eq!(
        code.url,
        format!("vscode://vscode-remote/ssh-remote+{authority}/home/Bob/%E9%A1%B9%E7%9B%AE")
    );
    assert_eq!(code.host, "Bob@Web.Example.com:2222");
    assert!(code.obstacles.contains(&Obstacle::Port { port: 2222 }));
    let zed = launch(Editor::Zed, &remote, "/home/Bob", "");
    assert_eq!(zed.url, "zed://ssh/Bob@Web.Example.com:2222/home/Bob");
    assert!(zed.obstacles.is_empty(), "Zed 认得链接中的端口");

    // IPv6 加方括号；用户名中的特殊字符转义
    let remote = ssh(Endpoint {
        address: "fe80::1".into(),
        port: 2200,
        user: "a b".into(),
        ..endpoint()
    });
    assert_eq!(
        launch(Editor::Zed, &remote, "/", "").url,
        "zed://ssh/a%20b@[fe80::1]:2200/"
    );
}

// 配置中有对应的别名时用别名（不再问），按 HostName、Port、User 匹配，含 Host * 的默认值与 %h
#[test]
fn links_with_alias() {
    let config = "\
Host *
    User alice

Host other
    HostName 10.0.0.6

Host Web-Prod web2
    HostName 10.0.0.5
";
    let remote = ssh(endpoint());
    let code = launch(Editor::VsCode, &remote, "/srv", config);
    assert_eq!(code.alias.as_deref(), Some("Web-Prod"));
    assert!(!code.needs_confirm());
    assert!(code.entry.is_none());
    // 别名含大写字母：写成十六进制 JSON
    assert_eq!(
        code.url,
        format!(
            "vscode://vscode-remote/ssh-remote+{}/srv",
            hex(r#"{"hostName":"Web-Prod"}"#)
        )
    );
    let zed = launch(Editor::Zed, &remote, "/srv", config);
    assert_eq!(zed.url, "zed://ssh/Web-Prod/srv");

    // 小写别名原样
    let config = "Host web\n  HostName=10.0.0.5\n  User alice\n";
    assert_eq!(
        launch(Editor::VsCode, &remote, "/srv", config).url,
        "vscode://vscode-remote/ssh-remote+web/srv"
    );
}

#[test]
fn alias_matching_rules() {
    let config = SshConfig::parse(
        "\
# 注释
User global-user
Host a
    HostName %h.example.com
Host b
    HostName 10.0.0.5
    Port 2222
Host c
    HostName 10.0.0.5
    User root
Host d *.internal !skip.internal
    HostName 10.0.0.7
Match host x
    User nobody
Host e
    HostName [fe80::1]
Host wild*
    HostName 10.0.0.9
",
    );
    assert_eq!(config.aliases(), vec!["a", "b", "c", "d", "e"]);
    // %h 展开为别名；第一个块之前的 User 对所有主机生效
    assert_eq!(
        config.find_alias("a.example.com", 22, "global-user", "me"),
        Some("a".into())
    );
    assert_eq!(config.find_alias("a.example.com", 22, "me", "me"), None);
    assert_eq!(
        config.find_alias("10.0.0.5", 2222, "global-user", "me"),
        Some("b".into())
    );
    // 第一个 Host 之前的 User 先出现，c 自己写的 User 不生效（与 ssh 相同）
    assert_eq!(
        config.find_alias("10.0.0.5", 22, "global-user", "me"),
        Some("c".into())
    );
    assert_eq!(config.find_alias("10.0.0.5", 22, "root", "me"), None);
    // 地址不区分大小写，用户名区分
    assert_eq!(
        config.find_alias("A.EXAMPLE.COM", 22, "global-user", "me"),
        Some("a".into())
    );
    assert_eq!(config.find_alias("10.0.0.5", 22, "Global-User", "me"), None);
    let later = SshConfig::parse("Host c\n HostName 10.0.0.5\n User root\nHost *\n User other\n");
    assert_eq!(
        later.find_alias("10.0.0.5", 22, "root", "me"),
        Some("c".into())
    );
    // Match 块不参与；IPv6 方括号
    assert_eq!(
        config.find_alias("10.0.0.7", 22, "global-user", "me"),
        Some("d".into())
    );
    assert_eq!(
        config.find_alias("fe80::1", 22, "global-user", "me"),
        Some("e".into())
    );
    // 通配的 Host 不是别名
    assert_eq!(config.find_alias("10.0.0.9", 22, "global-user", "me"), None);
    assert!(config.has_alias("A"));
    assert!(!config.has_alias("wild1"));

    // 没有 User 时按本机的用户名
    let config = SshConfig::parse("Host box\n HostName 10.0.0.5\n");
    assert_eq!(
        config.find_alias("10.0.0.5", 22, "me", "me"),
        Some("box".into())
    );
    assert_eq!(config.find_alias("10.0.0.5", 22, "alice", "me"), None);
}

// 建议的条目：termlet- 加名称，与已有的不重复；带 Port、IdentityFile、CertificateFile、ProxyJump，不带密码与代理命令
#[test]
fn suggested_entry() {
    let mut host = endpoint();
    host.port = 2222;
    host.identity_file = Some(PathBuf::from("/keys/work key"));
    host.certificate_file = Some(PathBuf::from("/certs/work-cert.pub"));
    host.jumps = vec![
        JumpHop {
            label: "bastion".into(),
            address: "bastion.example".into(),
            port: 22,
            user: "ops".into(),
        },
        JumpHop {
            label: "inner".into(),
            address: "fe80::2".into(),
            port: 2200,
            user: "ops".into(),
        },
    ];
    host.proxy = Some(ProxyKind::Command);
    let config = SshConfig::parse("Host termlet-web\n HostName 1.1.1.1\n");
    let entry = Entry::for_endpoint(&host, &config, STAMP);
    assert_eq!(entry.alias, "termlet-web-2", "与已有的别名不重复");
    assert_eq!(
        entry.text,
        "\
# Termlet 2026-11-09 18:00: Web 生产
Host termlet-web-2
    HostName 10.0.0.5
    User alice
    Port 2222
    IdentityFile \"/keys/work key\"
    CertificateFile /certs/work-cert.pub
    ProxyJump ops@bastion.example,ops@[fe80::2]:2200
"
    );
    // 名称没有能用的字符时用地址
    let named = Endpoint {
        name: "生产".into(),
        ..endpoint()
    };
    assert_eq!(
        Entry::for_endpoint(&named, &SshConfig::default(), STAMP).alias,
        "termlet-10.0.0.5"
    );
    // 写入后，新的配置能按地址找到这个别名
    let mut text = String::from("Host other\n HostName 9.9.9.9\n\n");
    text.push_str(&entry.text);
    assert_eq!(
        SshConfig::parse(&text).find_alias("10.0.0.5", 2222, "alice", "me"),
        Some("termlet-web-2".into())
    );
}

// 可能连不上的原因：跳板机、代理、代理命令、保存在 Termlet 中的私钥、不在默认位置的私钥文件、VS Code 的非 22 端口；
// 写入条目能解决的只有跳板机、私钥文件、端口
#[test]
fn obstacles_for_direct_links() {
    let mut host = endpoint();
    host.jumps = vec![JumpHop {
        label: "bastion".into(),
        address: "b".into(),
        port: 22,
        user: "ops".into(),
    }];
    host.proxy = Some(ProxyKind::Socks5);
    host.stored_key = true;
    host.identity_file = Some(PathBuf::from("/keys/k"));
    host.port = 2022;
    let code = launch(Editor::VsCode, &ssh(host.clone()), "/", "");
    assert_eq!(
        code.obstacles,
        vec![
            Obstacle::Jump {
                hosts: vec!["bastion".into()]
            },
            Obstacle::Proxy,
            Obstacle::StoredKey,
            Obstacle::KeyFile {
                path: "/keys/k".into()
            },
            Obstacle::Port { port: 2022 },
        ]
    );
    let fixed: Vec<bool> = code
        .obstacles
        .iter()
        .map(Obstacle::fixed_by_entry)
        .collect();
    assert_eq!(fixed, vec![true, false, false, true, true]);
    host.proxy = Some(ProxyKind::Command);
    let zed = launch(Editor::Zed, &ssh(host), "/", "");
    assert!(zed.obstacles.contains(&Obstacle::ProxyCommand));
    assert!(
        !zed.obstacles
            .iter()
            .any(|o| matches!(o, Obstacle::Port { .. }))
    );
    // 默认位置的私钥文件不算
    if let Some(home) = dirs::home_dir() {
        let host = Endpoint {
            identity_file: Some(home.join(".ssh").join("id_ed25519")),
            ..endpoint()
        };
        assert!(
            launch(Editor::Zed, &ssh(host), "/", "")
                .obstacles
                .is_empty()
        );
    }
}

// WSL：只有 VS Code 能打开
#[test]
fn wsl_links() {
    let remote = Remote::Wsl {
        distro: "Ubuntu-24.04".into(),
    };
    let code = launch(Editor::VsCode, &remote, "/home/me/a b", "");
    assert_eq!(
        code.url,
        "vscode://vscode-remote/wsl+Ubuntu-24.04/home/me/a%20b"
    );
    assert!(!code.needs_confirm());
    assert!(
        plan(
            Editor::Zed,
            &remote,
            "/",
            &SshConfig::default(),
            "me",
            STAMP
        )
        .is_none()
    );
}

// 从连接计划取出：目标的地址、用户、端口与私钥文件，跳板机，第一跳的代理，保存在 Termlet 中的私钥（任何一跳）
#[test]
fn endpoint_from_plan() {
    use crate::secret::SecretString;
    let hop = |label: &str, auth: AuthPlan, proxy: Option<ProxyPlan>| Hop {
        label: label.into(),
        host_id: None,
        address: format!("{label}.example"),
        port: 2200,
        username: "ops".into(),
        auth,
        totp: None,
        proxy,
        legacy_algorithms: false,
    };
    let proxy = ProxyPlan {
        kind: ProxyKind::Http,
        address: "proxy".into(),
        port: 8080,
        username: String::new(),
        password: None,
        command: String::new(),
        alias: String::new(),
        from_group: None,
    };
    let plan = ConnectPlan {
        hops: vec![
            hop(
                "bastion",
                AuthPlan::Key {
                    key: KeyMaterial::Text(SecretString::from("秘密".to_string())),
                    passphrase: None,
                    label: "k".into(),
                    certificate: None,
                },
                Some(proxy),
            ),
            hop(
                "web",
                AuthPlan::Key {
                    key: KeyMaterial::File(PathBuf::from("/keys/web")),
                    passphrase: None,
                    label: "web".into(),
                    certificate: None,
                },
                None,
            ),
        ],
    };
    let endpoint = Endpoint::from_plan("Web", &plan);
    assert_eq!(endpoint.address, "web.example");
    assert_eq!(endpoint.port, 2200);
    assert_eq!(endpoint.user, "ops");
    assert_eq!(endpoint.identity_file, Some(PathBuf::from("/keys/web")));
    assert_eq!(endpoint.jumps.len(), 1);
    assert_eq!(endpoint.jumps[0].label, "bastion");
    assert_eq!(endpoint.proxy, Some(ProxyKind::Http));
    assert!(endpoint.stored_key);
    assert!(!format!("{endpoint:?}").contains("秘密"), "不含秘密");
}

// 追加：只在末尾写，先备份；原文件不以换行结尾时补上，CRLF 的文件用 CRLF；文件不存在时新建
#[test]
fn append_keeps_existing_content_and_backs_up() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config");
    std::fs::write(&path, "Host a\r\n  HostName 1.1.1.1").unwrap();
    let entry = "# Termlet\nHost termlet-web\n    HostName 10.0.0.5\n";
    let Appended { backup } = append(&path, entry, "20261109-180000").unwrap();
    let backup = backup.expect("有备份");
    assert_eq!(
        backup.file_name().unwrap().to_string_lossy(),
        "config.termlet-backup-20261109-180000"
    );
    assert_eq!(
        std::fs::read_to_string(&backup).unwrap(),
        "Host a\r\n  HostName 1.1.1.1"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "Host a\r\n  HostName 1.1.1.1\r\n\r\n# Termlet\r\nHost termlet-web\r\n    HostName 10.0.0.5\r\n"
    );
    // 同一秒再写一次：备份不覆盖之前的
    let second = append(&path, "Host b\n", "20261109-180000").unwrap();
    assert_eq!(
        second
            .backup
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy(),
        "config.termlet-backup-20261109-180000-2"
    );

    let fresh = dir.path().join("new").join(".ssh").join("config");
    let created = append(&fresh, entry, "x").unwrap();
    assert_eq!(created.backup, None);
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), entry);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn detection_helpers() {
    assert_eq!(
        command_program(r#""C:\Programs\Microsoft VS Code\Code.exe" "--open-url" "--" "%1""#),
        Some(PathBuf::from(r"C:\Programs\Microsoft VS Code\Code.exe"))
    );
    assert_eq!(
        command_program(r"C:\Zed\Zed.exe %1"),
        Some(PathBuf::from(r"C:\Zed\Zed.exe"))
    );
    assert_eq!(command_program("  "), None);

    let dir = tempfile::tempdir().unwrap();
    let apps = dir.path().join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    std::fs::write(
        apps.join("code-url-handler.desktop"),
        "[Desktop Entry]\nName=Visual Studio Code - URL Handler\nMimeType=x-scheme-handler/vscode;\n",
    )
    .unwrap();
    std::fs::write(apps.join("notes.txt"), "MimeType=x-scheme-handler/zed;\n").unwrap();
    let dirs = vec![dir.path().join("missing"), apps];
    assert!(has_scheme_handler(&dirs, "vscode"));
    assert!(!has_scheme_handler(&dirs, "zed"), "只看桌面文件");

    std::fs::create_dir_all(dir.path().join("Zed Preview.app")).unwrap();
    let roots = vec![dir.path().to_path_buf()];
    assert!(has_app_bundle(&roots, mac_bundles(Editor::Zed)));
    assert!(!has_app_bundle(&roots, mac_bundles(Editor::VsCode)));
}

#[test]
fn editor_setting_filters() {
    use crate::model::RemoteEditorSetting as S;
    assert!(Editor::VsCode.allowed_by(S::Auto) && Editor::Zed.allowed_by(S::Auto));
    assert!(Editor::VsCode.allowed_by(S::VsCode) && !Editor::Zed.allowed_by(S::VsCode));
    assert!(!Editor::VsCode.allowed_by(S::Zed) && Editor::Zed.allowed_by(S::Zed));
    assert!(!Editor::VsCode.allowed_by(S::Off) && !Editor::Zed.allowed_by(S::Off));
}
