//! 密钥库服务层的测试：导入、生成、导出、改用密钥、部署（部署的集成测试连真实 sshd：
//! 设置 `TERMLET_TEST_SSH=127.0.0.1:2222` 后运行，否则跳过）。
//! 部署脚本本身的测试在 `sshkey::deploy`。

use std::sync::Arc;

use super::*;
use crate::model::{CliCredential, Profile};
use crate::ssh::{Answer, Prompter, no_prompt};
use crate::testkit::{accept_host_key, block_on, memory_store, test_server};

/// 一把 Ed25519 私钥的 OpenSSH 文本；`passphrase` 不为空时加密。
fn openssh_key(comment: &str, passphrase: &str) -> (String, String) {
    let key = sshkey::generate(KeyKind::Ed25519, comment).unwrap();
    let text = key
        .export_openssh(Some(&SecretString::new(passphrase)))
        .unwrap();
    (text.expose().to_string(), key.fingerprint())
}

fn text(content: &str) -> ImportSource {
    ImportSource::Text(SecretString::new(content))
}

async fn import_text(store: &Arc<Store>, content: &str) -> Result<Imported, KeyVaultError> {
    import(
        store.clone(),
        ImportRequest {
            name: String::new(),
            source: text(content),
            passphrase: None,
        },
    )
    .await
}

fn entry(store: &Store, id: &KeyId) -> KeyEntry {
    store.get::<KeyEntry>(id).unwrap().doc
}

fn stored_private_key(store: &Store, id: &KeyId) -> Option<SecretString> {
    store
        .secret(&Owner::key(id), SecretField::PrivateKey)
        .unwrap()
}

#[test]
fn import_rename_delete() {
    block_on(async {
        let store = memory_store();
        let (content, fingerprint) = openssh_key("deploy@laptop", "");
        let id = import_text(&store, &content).await.unwrap();
        let Imported::Added(id) = id else {
            panic!("应为新增：{id:?}")
        };
        let saved = entry(&store, &id);
        assert_eq!(saved.name, "deploy@laptop");
        assert_eq!(saved.algorithm, "ssh-ed25519");
        assert_eq!(saved.fingerprint, fingerprint);
        assert!(saved.public_key.starts_with("ssh-ed25519 AAAA"));
        assert_eq!(
            import_text(&store, &content).await,
            Err(KeyVaultError::AlreadyExists {
                key: id.clone(),
                name: "deploy@laptop".into()
            }),
            "重复导入应返回已存在"
        );

        // 加密私钥：缺少密码、密码错误、正确密码（从文件导入，名称取文件名）
        let (encrypted, _) = openssh_key("", "s3cret");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_work");
        std::fs::write(&path, &encrypted).unwrap();
        let from_file = |passphrase: Option<&str>| ImportRequest {
            name: String::new(),
            source: ImportSource::File(path.clone()),
            passphrase: passphrase.map(SecretString::new),
        };
        assert_eq!(
            import(store.clone(), from_file(None)).await,
            Err(KeyVaultError::Key(KeyError::PassphraseRequired))
        );
        assert_eq!(
            import(store.clone(), from_file(Some("x"))).await,
            Err(KeyVaultError::Key(KeyError::WrongPassphrase))
        );
        let work = import(store.clone(), from_file(Some("s3cret")))
            .await
            .unwrap();
        let work = work.id().clone();
        assert_eq!(entry(&store, &work).name, "id_work");
        // 私钥解密后存为不加密的 OpenSSH 格式，之后连接不再需要密码
        let private = stored_private_key(&store, &work).unwrap();
        assert!(private.expose().contains("BEGIN OPENSSH PRIVATE KEY"));
        sshkey::parse(private.expose().as_bytes(), None).expect("保存的私钥应可直接解析");

        // 无效内容、文件不存在
        assert_eq!(
            import_text(&store, "not a key").await,
            Err(KeyVaultError::Key(KeyError::Malformed(
                Malformed::Unrecognized
            )))
        );
        let missing = dir.path().join("nope");
        assert_eq!(
            import(
                store.clone(),
                ImportRequest {
                    name: String::new(),
                    source: ImportSource::File(missing.clone()),
                    passphrase: None
                }
            )
            .await,
            Err(KeyVaultError::FileNotFound { path: missing })
        );

        rename(store.clone(), id.clone(), " 部署 ".into())
            .await
            .unwrap();
        assert_eq!(entry(&store, &id).name, "部署");
        assert_eq!(
            rename(store.clone(), id.clone(), " ".into()).await,
            Err(KeyVaultError::Invalid(Invalid::new("name", "required")))
        );
        assert_eq!(
            rename(store.clone(), id.clone(), "名".repeat(65)).await,
            Err(KeyVaultError::Invalid(Invalid::new("name", "too_long")))
        );
        assert_eq!(store.list::<KeyEntry>().len(), 2);

        delete(store.clone(), work.clone()).await.unwrap();
        assert!(store.get::<KeyEntry>(&work).is_none());
        assert!(
            !store.has_secret(&Owner::key(&work), SecretField::PrivateKey),
            "删除密钥后私钥也应删除"
        );
        assert_eq!(
            delete(store.clone(), work).await,
            Err(KeyVaultError::NotFound)
        );
    });
}

/// 多台主机、凭据档案共用一把密钥；仍被使用的不能删除。
#[test]
fn shared_key_cannot_be_deleted() {
    block_on(async {
        let store = memory_store();
        let (content, _) = openssh_key("shared", "");
        let key = import_text(&store, &content).await.unwrap().id().clone();
        let library = || Auth::Key {
            source: KeySource::Library { key: key.clone() },
        };
        let host = |name: &str, auth: Auth| Host {
            name: name.into(),
            address: format!("{name}.example"),
            username: "u".into(),
            auth,
            ..Default::default()
        };
        let a = store
            .create(host("a", library()), None, Position::Last)
            .unwrap();
        // b 的认证是密码，命令行工具专用凭据用这把密钥
        let mut b_doc = host("b", Auth::Password);
        b_doc.cli.credential = Some(CliCredential {
            auth: CliAuth::Key {
                source: KeySource::Library { key: key.clone() },
            },
            ..Default::default()
        });
        let b = store.create(b_doc, None, Position::Last).unwrap();
        let profile = store
            .create(
                Profile {
                    name: "运维".into(),
                    auth: ProfileAuth::Key {
                        source: KeySource::Library { key: key.clone() },
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();

        let used = usage(&store, &key);
        assert_eq!(used.hosts.len(), 2);
        assert!(used.hosts.contains(&a) && used.hosts.contains(&b));
        assert_eq!(used.profiles, std::slice::from_ref(&profile));
        assert_eq!(
            delete(store.clone(), key.clone()).await,
            Err(KeyVaultError::InUse {
                hosts: 2,
                profiles: 1
            })
        );

        // 不再被使用后可以删除；主机删除不影响密钥
        store.delete::<Host>(&a).unwrap();
        store.delete::<Host>(&b).unwrap();
        store.delete::<Profile>(&profile).unwrap();
        assert!(
            stored_private_key(&store, &key).is_some(),
            "主机删除不应删除密钥库中的私钥"
        );
        assert!(usage(&store, &key).is_empty());
        delete(store.clone(), key).await.unwrap();
    });
}

/// 新密钥存入密钥库；同一把密钥（指纹相同）再次导入时返回已有的那把，不报错、不重复。
#[test]
fn ensure_returns_existing() {
    block_on(async {
        let store = memory_store();
        let (content, fingerprint) = openssh_key("", "");
        let id = ensure(store.clone(), "长".repeat(70), SecretString::new(&content))
            .await
            .unwrap();
        let saved = entry(&store, &id);
        assert_eq!(saved.fingerprint, fingerprint);
        assert_eq!(saved.name.chars().count(), 64, "名称应截断为 64 个字符");
        let again = ensure(
            store.clone(),
            "另一个名字".into(),
            SecretString::new(&content),
        )
        .await
        .unwrap();
        assert_eq!(again, id);
        assert_eq!(entry(&store, &id).name, saved.name);
        assert_eq!(store.list::<KeyEntry>().len(), 1, "不应重复保存");
        assert!(
            ensure(store.clone(), "x".into(), SecretString::new("not a key"))
                .await
                .is_err()
        );
    });
}

/// 保险库重置后密钥库中的私钥为空，再导入同一把密钥时补上，ID、名称不变；
/// 私钥完好时仍返回已存在；密文损坏时同样补上；ensure 同样补上。
#[test]
fn import_restores_lost_private_key() {
    block_on(async {
        let store = memory_store();
        let (content, _) = openssh_key("deploy", "");
        let (other, _) = openssh_key("other", "");
        let key = import_text(&store, &content).await.unwrap().id().clone();
        let other_key = import_text(&store, &other).await.unwrap().id().clone();

        // 主密钥丢失后重置保险库：秘密全部清空
        store.reset_vault(None).unwrap();
        assert!(stored_private_key(&store, &key).is_none());
        assert_eq!(
            private_key(&store, &key).unwrap_err(),
            KeyVaultError::PrivateKeyMissing
        );

        let restored = import(
            store.clone(),
            ImportRequest {
                name: "新名字".into(),
                source: text(&content),
                passphrase: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(restored, Imported::Restored(key.clone()));
        assert_eq!(entry(&store, &key).name, "deploy", "名称不变");
        assert!(private_key(&store, &key).is_ok(), "补上私钥后应可用");
        assert!(matches!(
            import_text(&store, &content).await,
            Err(KeyVaultError::AlreadyExists { .. })
        ));

        // 私钥的密文损坏（无法解密）
        let owner = Owner::key(&key).to_string();
        store
            .database()
            .write(|tx| {
                tx.execute(
                    "UPDATE secret SET blob = ?1 WHERE owner = ?2",
                    rusqlite::params![b"garbage-garbage-garbage".to_vec(), owner],
                )?;
                Ok(())
            })
            .unwrap();
        assert_eq!(
            import_text(&store, &content).await,
            Ok(Imported::Restored(key.clone()))
        );
        assert!(private_key(&store, &key).is_ok());

        // ensure 同样补上
        let ensured = ensure(store.clone(), "x".into(), SecretString::new(&other))
            .await
            .unwrap();
        assert_eq!(ensured, other_key);
        assert!(private_key(&store, &other_key).is_ok());
        assert_eq!(store.list::<KeyEntry>().len(), 2, "不应新增密钥");
    });
}

/// 生成的密钥与导入的一样存入密钥库，名称默认取注释，私钥可直接解析。
#[test]
fn generate_keys() {
    block_on(async {
        let store = memory_store();
        let request = |name: &str, kind: KeyKind, comment: &str| GenerateRequest {
            name: name.into(),
            kind,
            comment: comment.into(),
        };
        let id = generate(store.clone(), request("", KeyKind::Ed25519, "ruan@laptop"))
            .await
            .unwrap();
        let saved = entry(&store, &id);
        assert_eq!(saved.name, "ruan@laptop");
        assert_eq!(saved.algorithm, "ssh-ed25519");
        assert!(saved.public_key.ends_with(" ruan@laptop"));
        let parsed = private_key(&store, &id).unwrap();
        assert_eq!(parsed.fingerprint(), saved.fingerprint);
        assert_eq!(parsed.public_key(), saved.public_key);

        let rsa = generate(store.clone(), request("部署用", KeyKind::Rsa3072, ""))
            .await
            .unwrap();
        let saved = entry(&store, &rsa);
        assert_eq!(
            (saved.name.as_str(), saved.algorithm.as_str(), saved.bits),
            ("部署用", "ssh-rsa", 3072)
        );
        assert_eq!(
            saved.public_key.matches(' ').count(),
            1,
            "没有注释时公钥行只有两段"
        );
        // 没有名称和注释：用"类型 + 指纹前几位"
        let unnamed = generate(store.clone(), request("", KeyKind::Ed25519, ""))
            .await
            .unwrap();
        assert!(entry(&store, &unnamed).name.starts_with("ed25519 "));

        assert_eq!(
            generate(store.clone(), request("", KeyKind::Ed25519, "a\nb")).await,
            Err(KeyVaultError::Invalid(Invalid::new(
                "comment",
                "control_characters"
            )))
        );
        assert_eq!(
            generate(
                store.clone(),
                request(&"名".repeat(65), KeyKind::Ed25519, "")
            )
            .await,
            Err(KeyVaultError::Invalid(Invalid::new("name", "too_long")))
        );
        assert_eq!(store.list::<KeyEntry>().len(), 3);
    });
}

/// OpenSSH 格式、可选私钥密码、绝对路径、已存在时需要确认覆盖；公钥导出。
#[test]
fn export_keys() {
    block_on(async {
        let store = memory_store();
        let id = generate(
            store.clone(),
            GenerateRequest {
                comment: "export-me".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let fingerprint = entry(&store, &id).fingerprint;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id_export");
        let request = |path: &Path, passphrase: Option<&str>, overwrite: bool| ExportRequest {
            path: path.to_path_buf(),
            passphrase: passphrase.map(SecretString::new),
            overwrite,
        };

        let written = export_private_key(store.clone(), id.clone(), request(&path, None, false))
            .await
            .unwrap();
        assert_eq!(written, path);
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.contains("BEGIN OPENSSH PRIVATE KEY"));
        let parsed = sshkey::parse(data.as_bytes(), None).unwrap();
        assert_eq!(parsed.fingerprint(), fingerprint);
        assert_eq!(parsed.comment(), "export-me");

        // 已存在：不确认时拒绝，确认后覆盖（这次带私钥密码）
        assert_eq!(
            export_private_key(store.clone(), id.clone(), request(&path, Some("pw"), false)).await,
            Err(KeyVaultError::FileExists { path: path.clone() })
        );
        export_private_key(store.clone(), id.clone(), request(&path, Some("pw"), true))
            .await
            .unwrap();
        let data = std::fs::read(&path).unwrap();
        assert_eq!(
            sshkey::parse(&data, None).unwrap_err(),
            KeyError::PassphraseRequired,
            "带密码导出后应要求密码"
        );
        let parsed = sshkey::parse(&data, Some(&SecretString::new("pw"))).unwrap();
        assert_eq!(parsed.fingerprint(), fingerprint);

        assert_eq!(
            export_private_key(
                store.clone(),
                id.clone(),
                request(Path::new("relative/id"), None, false)
            )
            .await,
            Err(KeyVaultError::NotAbsolute)
        );
        assert_eq!(
            export_private_key(
                store.clone(),
                id.clone(),
                request(Path::new(""), None, false)
            )
            .await,
            Err(KeyVaultError::NotAbsolute)
        );
        assert_eq!(
            export_private_key(store.clone(), id.clone(), request(dir.path(), None, true)).await,
            Err(KeyVaultError::IsDirectory {
                path: dir.path().to_path_buf()
            })
        );
        let nested = dir.path().join("missing").join("id");
        assert_eq!(
            export_private_key(store.clone(), id.clone(), request(&nested, None, false)).await,
            Err(KeyVaultError::DirectoryMissing {
                path: dir.path().join("missing")
            })
        );
        assert_eq!(
            export_private_key(
                store.clone(),
                KeyId::from_raw("k_nope"),
                request(&dir.path().join("x"), None, false)
            )
            .await,
            Err(KeyVaultError::NotFound)
        );

        // 公钥
        let public = dir.path().join("id_export.pub");
        export_public_key(store.clone(), id.clone(), public.clone(), false)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&public).unwrap(),
            format!("{}\n", entry(&store, &id).public_key)
        );
        assert!(matches!(
            export_public_key(store.clone(), id, public, false).await,
            Err(KeyVaultError::FileExists { .. })
        ));
    });
}

/// 只改认证方式，其他设置与命令行工具专用凭据不变，原来的密码删除；agent 主机也可改用。
#[test]
fn switch_host_to_library_key() {
    block_on(async {
        let store = memory_store();
        let key = generate(
            store.clone(),
            GenerateRequest {
                comment: "lib".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut doc = Host {
            name: "a".into(),
            address: "a.example".into(),
            username: "u".into(),
            tags: vec!["prod".into()],
            note: "备注".into(),
            favorite: true,
            ..Default::default()
        };
        doc.cli.allowed = true;
        doc.cli.credential = Some(CliCredential {
            username: "ro".into(),
            auth: CliAuth::Password,
        });
        let host = store.create(doc.clone(), None, Position::Last).unwrap();
        let owner = Owner::host(&host);
        for (field, value) in [
            (SecretField::Password, "pw"),
            (SecretField::CliPassword, "cli"),
            (SecretField::Totp, "JBSWY3DPEHPK3PXP"),
        ] {
            store
                .set_secret(&owner, field, Some(&SecretString::new(value)))
                .unwrap();
        }

        use_library_key(store.clone(), host.clone(), key.clone())
            .await
            .unwrap();
        let after = store.get::<Host>(&host).unwrap().doc;
        assert_eq!(
            after.auth,
            Auth::Key {
                source: KeySource::Library { key: key.clone() }
            }
        );
        assert_eq!(
            Host {
                auth: doc.auth.clone(),
                ..after
            },
            doc,
            "只应修改认证方式"
        );
        assert!(
            !store.has_secret(&owner, SecretField::Password),
            "原来的密码应删除"
        );
        assert!(store.has_secret(&owner, SecretField::CliPassword));
        assert!(store.has_secret(&owner, SecretField::Totp));

        let agent = store
            .create(
                Host {
                    name: "b".into(),
                    address: "b.example".into(),
                    auth: Auth::Agent,
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        use_library_key(store.clone(), agent.clone(), key.clone())
            .await
            .unwrap();
        assert_eq!(usage(&store, &key).hosts.len(), 2);

        // 用户名来自凭据档案：写进主机，登录的账号不变
        let profile = store
            .create(
                Profile {
                    name: "运维".into(),
                    username: "ops".into(),
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let by_profile = store
            .create(
                Host {
                    name: "c".into(),
                    address: "c.example".into(),
                    auth: Auth::Profile { profile },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        use_library_key(store.clone(), by_profile.clone(), key.clone())
            .await
            .unwrap();
        assert_eq!(store.get::<Host>(&by_profile).unwrap().doc.username, "ops");

        assert_eq!(
            use_library_key(store.clone(), agent, KeyId::from_raw("k_nope")).await,
            Err(KeyVaultError::NotFound)
        );
        assert_eq!(
            use_library_key(store.clone(), HostId::from_raw("h_nope"), key).await,
            Err(KeyVaultError::HostNotFound)
        );
    });
}

#[test]
fn deploy_checks_inputs() {
    block_on(async {
        let store = memory_store();
        let key = generate(store.clone(), GenerateRequest::default())
            .await
            .unwrap();
        let request = |key: KeyId, host: HostId| DeployRequest {
            key,
            host,
            use_key: false,
            prompter: no_prompt(),
        };
        let host = store
            .create(
                Host {
                    name: "a".into(),
                    address: "a.example".into(),
                    protocol: crate::model::Protocol::Telnet,
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        assert_eq!(
            deploy(
                store.clone(),
                request(KeyId::from_raw("k_nope"), host.clone())
            )
            .await,
            Err(KeyVaultError::NotFound)
        );
        assert_eq!(
            deploy(
                store.clone(),
                request(key.clone(), HostId::from_raw("h_nope"))
            )
            .await,
            Err(KeyVaultError::HostNotFound)
        );
        assert_eq!(
            deploy(store.clone(), request(key, host)).await,
            Err(KeyVaultError::NotSsh)
        );
    });
}

#[test]
fn debug_hides_secrets() {
    let request = ImportRequest {
        name: "n".into(),
        source: text("-----BEGIN OPENSSH PRIVATE KEY-----"),
        passphrase: Some(SecretString::new("hunter2")),
    };
    let debug = format!("{request:?}");
    assert!(
        !debug.contains("hunter2") && !debug.contains("BEGIN"),
        "{debug}"
    );
}

// ---------------------------------------------------------------- 部署（真实 sshd）

/// 未确认指纹时（不询问）失败 → 部署并只用该密钥验证登录、改用密钥 →
/// 重复部署不重复添加 → 注释中的特殊字符不会在远端执行 → 主机凭据错误时失败 → 结束时清理 authorized_keys 中的测试行。
#[test]
fn deploy_to_real_sshd() {
    let Some((address, port)) = test_server() else {
        return;
    };
    block_on(async {
        let store = memory_store();
        let marker = format!("termlet-deploy-test-{}", crate::id::random_token(4));
        let pwned = format!("/tmp/{marker}");
        let key = generate(
            store.clone(),
            GenerateRequest {
                name: "deploy-test".into(),
                kind: KeyKind::Ed25519,
                comment: format!(r#"{marker} it's $(touch {pwned}) "q" \ ;"#),
            },
        )
        .await
        .unwrap();
        let add_host = |name: &str, password: &str| {
            let id = store
                .create(
                    Host {
                        name: name.into(),
                        address: address.clone(),
                        port,
                        username: "termlet".into(),
                        ..Default::default()
                    },
                    None,
                    Position::Last,
                )
                .unwrap();
            store
                .set_secret(
                    &Owner::host(&id),
                    SecretField::Password,
                    Some(&SecretString::new(password)),
                )
                .unwrap();
            id
        };
        let host = add_host("e2e", "termlet-e2e");
        let request = |use_key: bool, prompter: Prompter| DeployRequest {
            key: key.clone(),
            host: host.clone(),
            use_key,
            prompter,
        };

        // 1. 还没确认过主机指纹、又不能询问：失败
        assert_eq!(
            deploy(store.clone(), request(true, no_prompt())).await,
            Err(KeyVaultError::Connect(SshError::HostKeyRejected {
                host: address.clone()
            }))
        );

        // 2. 部署、只用该密钥验证登录、改用此密钥（这次确认指纹）
        let report = deploy(store.clone(), request(true, accept_host_key()))
            .await
            .unwrap();
        assert_eq!(report.outcome, DeployOutcome::Added);
        assert!(report.verified(), "{report:?}");
        assert!(report.switched);
        assert_eq!(
            store.get::<Host>(&host).unwrap().doc.auth,
            Auth::Key {
                source: KeySource::Library { key: key.clone() }
            }
        );
        assert!(!store.has_secret(&Owner::host(&host), SecretField::Password));

        // 检查与清理用另一台保存了密码的主机执行
        let checker = add_host("checker", "termlet-e2e");
        let plan = crate::ssh::plan_for_host(&store, &checker).unwrap();
        let connection = crate::ssh::connect(
            store.clone(),
            &plan,
            no_prompt(),
            &crate::ssh::ConnectOptions::default(),
        )
        .await
        .unwrap();
        let run = |command: String| {
            let connection = connection.clone();
            async move {
                let result = crate::ssh::exec(
                    &connection,
                    &command,
                    64 * 1024,
                    std::time::Duration::from_secs(20),
                )
                .await
                .unwrap();
                String::from_utf8_lossy(&result.stdout).into_owned()
            }
        };

        // 3. 重复部署（这次用密钥登录）：不重复添加
        let report = deploy(store.clone(), request(false, no_prompt())).await;
        let check = run(format!(
            "grep -c '{marker}' ~/.ssh/authorized_keys; test -e {pwned} && echo PWNED; stat -c %a ~/.ssh ~/.ssh/authorized_keys; grep '{marker}' ~/.ssh/authorized_keys"
        ))
        .await;
        // 清理（无论上面的检查是否通过）
        run(format!(
            "sed -i '/{marker}/d' ~/.ssh/authorized_keys; rm -f {pwned}"
        ))
        .await;
        let report = report.unwrap();
        assert_eq!(report.outcome, DeployOutcome::AlreadyPresent);
        assert!(report.verified() && !report.switched, "{report:?}");
        let lines: Vec<&str> = check.lines().collect();
        assert_eq!(lines.len(), 4, "{check}");
        assert_eq!(
            lines[0], "1",
            "authorized_keys 中应只有一行测试公钥：{check}"
        );
        // 只比较权限位（容器中 home 目录带 setgid，新建的 .ssh 会继承，显示为 2700）
        assert!(
            lines[1].ends_with("700") && lines[2].ends_with("600"),
            "{check}"
        );
        assert_eq!(
            lines[3],
            entry(&store, &key).public_key,
            "写入的公钥行与密钥库不一致"
        );

        // 4. 主机凭据错误：部署失败并返回认证错误
        let bad = add_host("bad", "wrong");
        let error = deploy(
            store.clone(),
            DeployRequest {
                key: key.clone(),
                host: bad,
                use_key: false,
                prompter: no_prompt(),
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(error, KeyVaultError::Connect(_)), "{error:?}");

        // 5. 没有部署的密钥：只用私钥认证时直接失败，不会退回密码或键盘交互（不询问）
        let other = sshkey::generate(KeyKind::Ed25519, "").unwrap();
        let mut plan = crate::ssh::plan_for_host(&store, &checker).unwrap();
        plan.hops.last_mut().unwrap().auth = crate::ssh::plan::AuthPlan::Key {
            key: crate::ssh::plan::KeyMaterial::Text(other.to_openssh().unwrap()),
            passphrase: None,
            label: "other".into(),
            certificate: None,
        };
        let asked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recording: Prompter = {
            let asked = asked.clone();
            Arc::new(move |_| {
                asked.store(true, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { Answer::Cancel })
            })
        };
        let error = crate::ssh::connect(
            store.clone(),
            &plan,
            recording,
            &crate::ssh::ConnectOptions::default(),
        )
        .await
        .err()
        .expect("未部署的密钥不应能登录");
        assert!(matches!(error, SshError::AuthFailed { .. }), "{error:?}");
        assert!(!asked.load(std::sync::atomic::Ordering::SeqCst), "不应询问");
    });
}

/// 给 `key`（OpenSSH 私钥文本）签发一张用户证书，返回一行 OpenSSH 证书。
fn certificate_for(key_text: &str, kind: crate::sshcert::CertKind) -> String {
    let key = sshkey::parse(key_text.as_bytes(), None)
        .unwrap()
        .into_ssh_private_key();
    let ca = sshkey::generate(KeyKind::Ed25519, "")
        .unwrap()
        .into_ssh_private_key();
    crate::sshcert::issue(
        &ca,
        key.public_key().key_data(),
        kind,
        &["deploy"],
        0,
        u64::MAX,
    )
    .to_openssh()
    .unwrap()
}

// 附带证书：只接受这把密钥的用户证书；可以替换、去掉；证书随条目保存
#[test]
fn attach_and_detach_certificate() {
    use crate::sshcert::CertKind;
    block_on(async {
        let store = memory_store();
        let (content, _) = openssh_key("ops", "");
        let id = import_text(&store, &content).await.unwrap().id().clone();
        let attach =
            |source: CertificateSource| attach_certificate(store.clone(), id.clone(), source);

        let cert = certificate_for(&content, CertKind::User);
        attach(CertificateSource::Text(format!("{cert}\n")))
            .await
            .unwrap();
        assert_eq!(entry(&store, &id).certificate, cert);

        // 另一把密钥的证书、主机证书、不是证书的内容
        let (other, _) = openssh_key("", "");
        assert_eq!(
            attach(CertificateSource::Text(certificate_for(
                &other,
                CertKind::User
            )))
            .await,
            Err(KeyVaultError::CertificateMismatch)
        );
        assert_eq!(
            attach(CertificateSource::Text(certificate_for(
                &content,
                CertKind::Host
            )))
            .await,
            Err(KeyVaultError::HostCertificate)
        );
        let public = entry(&store, &id).public_key;
        assert_eq!(
            attach(CertificateSource::Text(public)).await,
            Err(KeyVaultError::Certificate(
                crate::sshcert::CertError::NotCertificate
            ))
        );
        assert_eq!(entry(&store, &id).certificate, cert, "失败时保留原来的");

        // 从文件附带；文件不存在
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id-cert.pub");
        let renewed = certificate_for(&content, CertKind::User);
        std::fs::write(&path, &renewed).unwrap();
        attach(CertificateSource::File(path.clone())).await.unwrap();
        assert_eq!(entry(&store, &id).certificate, renewed);
        assert!(matches!(
            attach(CertificateSource::File(dir.path().join("missing"))).await,
            Err(KeyVaultError::FileNotFound { .. })
        ));

        detach_certificate(store.clone(), id.clone()).await.unwrap();
        assert_eq!(entry(&store, &id).certificate, "");

        // 连接计划中密钥库的密钥带上证书
        let mut doc = entry(&store, &id);
        doc.certificate = renewed.clone();
        store.update(&id, doc).unwrap();
        let host = store
            .create(
                Host {
                    name: "web".into(),
                    address: "web.example".into(),
                    auth: Auth::Key {
                        source: KeySource::Library { key: id.clone() },
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        match &crate::ssh::plan_for_host(&store, &host)
            .unwrap()
            .target()
            .auth
        {
            crate::ssh::plan::AuthPlan::Key { certificate, .. } => assert_eq!(
                certificate,
                &Some(crate::ssh::CertMaterial::Text(renewed.clone()))
            ),
            other => panic!("{other:?}"),
        }
    });
}

// 从文件导入私钥时，旁边同名的 -cert.pub 是这把密钥的就一并附带；不是的不附带，导入照常
#[test]
fn import_attaches_certificate_beside_key_file() {
    use crate::sshcert::CertKind;
    block_on(async {
        let store = memory_store();
        let dir = tempfile::tempdir().unwrap();
        let import_file = |path: std::path::PathBuf| {
            import(
                store.clone(),
                ImportRequest {
                    name: String::new(),
                    source: ImportSource::File(path),
                    passphrase: None,
                },
            )
        };

        let (content, _) = openssh_key("with-cert", "");
        let path = dir.path().join("id_ed25519");
        std::fs::write(&path, &content).unwrap();
        let cert = certificate_for(&content, CertKind::User);
        std::fs::write(dir.path().join("id_ed25519-cert.pub"), &cert).unwrap();
        let id = import_file(path).await.unwrap().id().clone();
        assert_eq!(entry(&store, &id).certificate, cert);

        let (content, _) = openssh_key("wrong-cert", "");
        let path = dir.path().join("id_other");
        std::fs::write(&path, &content).unwrap();
        std::fs::write(dir.path().join("id_other-cert.pub"), &cert).unwrap();
        let id = import_file(path).await.unwrap().id().clone();
        assert_eq!(entry(&store, &id).certificate, "");
    });
}
