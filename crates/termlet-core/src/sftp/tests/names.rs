//! 远程文件名的编码对真实 sshd 的测试：设置 `TERMLET_TEST_SSH=127.0.0.1:2222` 后运行，否则跳过。
//! 用 printf 的八进制转义在服务器上建 GBK 字节的文件名，再经 SFTP 列出、上传、下载、编辑、改名、删除，
//! 用 ls 核对服务器上的原始字节。

use std::sync::Arc;
use std::time::Duration;

use encoding_rs::GB18030;

use super::*;
use crate::id::HostId;
use crate::model::Encoding;
use crate::sftp::{
    DownloadRequest, Rejection, SftpError, SshTarget, TransferState, UploadRequest,
    WriteTextRequest, raw_byte,
};
use crate::ssh::{TerminalSpec, exec};
use crate::store::Store;
use crate::testkit::{SSH_USER, accept_host_key, server_store};

struct Server {
    store: Arc<Store>,
    host: HostId,
}

fn setup() -> Option<Server> {
    let (store, host) = server_store("gbk", SSH_USER)?;
    Some(Server { store, host })
}

impl Server {
    async fn open(&self, m: &SftpManager, encoding: Encoding) -> Arc<SftpSession> {
        m.open_ssh(SshTarget {
            store: self.store.clone(),
            host: self.host.to_string(),
            spec: TerminalSpec::for_host(&self.store, &self.host).unwrap(),
            share_group: None,
            prompter: accept_host_key(),
            owner: "w1".into(),
            sudo: None,
            name_encoding: encoding,
        })
        .await
        .unwrap()
    }
}

fn gbk(text: &str) -> Vec<u8> {
    GB18030.encode(text).0.into_owned()
}

/// 字节写成 printf 的八进制转义。
fn octal(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("\\{b:03o}")).collect()
}

/// 在服务器上执行 sh 命令，返回标准输出。
async fn sh(s: &SftpSession, command: &str) -> Vec<u8> {
    let result = exec(
        &s.connection().unwrap(),
        command,
        64 * 1024,
        Duration::from_secs(20),
    )
    .await
    .unwrap();
    assert_eq!(result.exit_code, Some(0), "{command}: {:?}", result);
    result.stdout
}

/// 目录中各项的原始字节（ls -1 的每一行）。
async fn raw_names(s: &SftpSession, dir: &str) -> Vec<Vec<u8>> {
    let mut names: Vec<Vec<u8>> = sh(s, &format!("ls -1A {dir}"))
        .await
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    names.sort();
    names
}

// GBK 的主机：列出、上传、下载、编辑、改名、删除都按 GBK；解码不了的字节转义后也能删除
#[test]
fn gbk_names_on_a_real_server() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let s = server.open(&m, Encoding::Gb18030).await;
        let dir = format!("/tmp/termlet-gbk-{}", crate::id::random_token(6));
        s.mkdir(&dir).await.unwrap();
        // 服务器上建：中文.txt（GBK）、FF FE.bin（GBK 也解码不了）
        sh(
            &s,
            &format!(
                "printf 'hello' > \"{dir}/$(printf '{}')\" && printf 'x' > \"{dir}/$(printf '\\377\\376.bin')\"",
                octal(&gbk("中文.txt"))
            ),
        )
        .await;

        let listing = s.list(&dir).await.unwrap();
        let mut names: Vec<String> = listing.entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"中文.txt".to_string()), "{names:?}");
        let odd = names.iter().find(|n| n.ends_with(".bin")).unwrap().clone();
        assert_eq!(
            odd.chars().take(2).map(raw_byte).collect::<Vec<_>>(),
            [Some(0xFF), Some(0xFE)],
            "解码不了的字节转义"
        );

        // 在线编辑：读出、保存
        let text = s.read_text(&format!("{dir}/中文.txt"), None).await.unwrap();
        assert_eq!(text.content, "hello");
        s.write_text(&WriteTextRequest {
            path: format!("{dir}/中文.txt"),
            content: "你好".into(),
            encoding: None,
            expected_modified: None,
            expected_size: None,
            force: true,
        })
        .await
        .unwrap();

        // 改名：服务器上是 GBK 的新名称
        s.rename(&format!("{dir}/中文.txt"), &format!("{dir}/改名.txt"))
            .await
            .unwrap();
        // 删除转义的名称
        s.remove(&format!("{dir}/{odd}")).await.unwrap();
        assert_eq!(raw_names(&s, &dir).await, vec![gbk("改名.txt")]);
        assert_eq!(
            sh(
                &s,
                &format!("cat \"{dir}/$(printf '{}')\"", octal(&gbk("改名.txt")))
            )
            .await,
            "你好".as_bytes()
        );

        // 上传、下载
        let local = tempfile::tempdir().unwrap();
        let upload = local.path().join("上传.txt");
        std::fs::write(&upload, b"up").unwrap();
        let views = m
            .upload(
                &s,
                UploadRequest {
                    local_paths: vec![upload],
                    remote_dir: dir.clone(),
                    overwrite: false,
                },
            )
            .await
            .unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert_eq!(v.state, TransferState::Done, "{v:?}");
        let mut want = vec![gbk("上传.txt"), gbk("改名.txt")];
        want.sort();
        assert_eq!(raw_names(&s, &dir).await, want);
        let down = tempfile::tempdir().unwrap();
        let views = m
            .download(
                &s,
                DownloadRequest {
                    remote_paths: vec![format!("{dir}/改名.txt")],
                    local_dir: down.path().to_path_buf(),
                },
            )
            .await
            .unwrap();
        let v = wait_finished(&m, &views[0].id).await;
        assert_eq!(v.state, TransferState::Done, "{v:?}");
        assert_eq!(
            std::fs::read_to_string(down.path().join("改名.txt")).unwrap(),
            "你好"
        );

        s.remove(&dir).await.unwrap();
        m.close_all().await;
    });
}

// 默认的 UTF-8：GBK 的名称显示为转义的字节，照样能改名、删除（以前只能看到乱码、删不掉）
#[test]
fn utf8_host_keeps_gbk_bytes() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let s = server.open(&m, Encoding::Utf8).await;
        let dir = format!("/tmp/termlet-raw-{}", crate::id::random_token(6));
        s.mkdir(&dir).await.unwrap();
        sh(
            &s,
            &format!(
                "printf 'x' > \"{dir}/$(printf '{}')\"",
                octal(&gbk("中.txt"))
            ),
        )
        .await;
        let listing = s.list(&dir).await.unwrap();
        let name = listing.entries[0].name.clone();
        assert_eq!(
            name.chars().filter_map(raw_byte).collect::<Vec<_>>(),
            gbk("中"),
            "{name:?}"
        );
        assert!(name.ends_with(".txt"));
        let renamed = format!("{}-2.txt", name.trim_end_matches(".txt"));
        s.rename(&format!("{dir}/{name}"), &format!("{dir}/{renamed}"))
            .await
            .unwrap();
        assert_eq!(
            raw_names(&s, &dir).await,
            vec![[gbk("中"), b"-2.txt".to_vec()].concat()]
        );
        s.remove(&format!("{dir}/{renamed}")).await.unwrap();
        assert!(raw_names(&s, &dir).await.is_empty());
        s.remove(&dir).await.unwrap();
        m.close_all().await;
    });
}

// Big5 表示不了的名称：不发给服务器，报为名称不能表示
#[test]
fn big5_rejects_unencodable_names() {
    let Some(server) = setup() else {
        return;
    };
    block_on(async {
        let m = manager();
        let s = server.open(&m, Encoding::Big5).await;
        let result = s.mkdir("/tmp/表情😀").await;
        assert_eq!(result, Err(SftpError::Rejected(Rejection::UnencodableName)));
        m.close_all().await;
    });
}
