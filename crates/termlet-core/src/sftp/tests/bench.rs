//! 文件传输吞吐的基准（连真实 sshd，默认不跑，用发布构建）：
//! `TERMLET_TEST_SSH=127.0.0.1:2222 cargo test -p termlet-core --release --lib sftp::tests::bench -- --ignored --nocapture --test-threads 1`
//!
//! 同一个文件连续传若干次，打印每次的 MB/s：
//! - [`sftp_throughput`]：经传输管理器，同一个会话连续上传（覆盖）、连续下载（本机已有同名文件时自动改名，与文件面板
//!   相同），再每次新开会话下载；
//! - [`sftp_shared_throughput`]：SFTP 与终端共用一条连接（桌面端从终端打开文件管理时的做法），终端开着 shell；
//! - [`cli_throughput`]：命令行的 `put_file` / `get_file`（每次在连接上临时打开 SFTP）；
//! - [`scp_throughput`]：SCP 模式。
//!
//! `TERMLET_BENCH_MB`（默认 256）、`TERMLET_BENCH_RUNS`（默认 5）、`TERMLET_BENCH_DIR`（本机目录，默认系统临时目录）可改。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::id::HostId;
use crate::sftp::backend::Backend;
use crate::sftp::scp::ScpRemote;
use crate::sftp::session::Link;
use crate::sftp::{DownloadRequest, SshTarget, UploadRequest};
use crate::ssh::{ConnectionPool, ShellOptions, TerminalSpec, connect, open_shell};
use crate::store::Store;
use crate::testkit::{SSH_USER, accept_host_key, connect_test, server_store, test_server};

struct Bench {
    store: Arc<Store>,
    host: HostId,
    bytes: u64,
    runs: u64,
    /// 本机的测试文件
    local: PathBuf,
    /// 下载到这里
    out: PathBuf,
    _dir: tempfile::TempDir,
}

impl Bench {
    /// 没有设置测试服务器时为 `None`（跳过）。
    fn new(name: &str) -> Option<Self> {
        let (store, host) = server_store(name, SSH_USER)?;
        let mb = env_num("TERMLET_BENCH_MB", 256);
        let runs = env_num("TERMLET_BENCH_RUNS", 5);
        let dir = match std::env::var_os("TERMLET_BENCH_DIR") {
            Some(dir) => tempfile::tempdir_in(dir).unwrap(),
            None => tempfile::tempdir().unwrap(),
        };
        let src = dir.path().join("src");
        let out = dir.path().join("out");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let local = src.join("big.bin");
        make_file(&local, mb);
        println!(
            "{name}：文件 {mb} MB，各 {runs} 次，本机目录 {}",
            dir.path().display()
        );
        Some(Self {
            store,
            host,
            bytes: mb << 20,
            runs,
            local,
            out,
            _dir: dir,
        })
    }

    fn target(&self, share_group: Option<String>) -> SshTarget {
        SshTarget {
            store: self.store.clone(),
            host: self.host.to_string(),
            spec: TerminalSpec::for_host(&self.store, &self.host).unwrap(),
            share_group,
            prompter: accept_host_key(),
            owner: "w1".into(),
            sudo: None,
            name_encoding: crate::model::Encoding::Utf8,
        }
    }

    /// 经管理器上传测试文件到远程目录 `dir`（覆盖），返回用时。
    async fn upload(&self, m: &SftpManager, s: &Arc<SftpSession>, dir: &str) -> Duration {
        let start = Instant::now();
        let request = UploadRequest {
            local_paths: vec![self.local.clone()],
            remote_dir: dir.to_string(),
            overwrite: true,
        };
        let views = m.upload(s, request).await.unwrap();
        let v = finish(m, &views[0].id).await;
        assert!(v.state == TransferState::Done, "上传未完成：{v:?}");
        start.elapsed()
    }

    /// 经管理器下载 `remote` 到本机目录（同名时自动改名），返回用时。
    async fn download(&self, m: &SftpManager, s: &Arc<SftpSession>, remote: &str) -> Duration {
        let start = Instant::now();
        let request = DownloadRequest {
            remote_paths: vec![remote.to_string()],
            local_dir: self.out.clone(),
        };
        let views = m.download(s, request).await.unwrap();
        let v = finish(m, &views[0].id).await;
        assert!(v.state == TransferState::Done, "下载未完成：{v:?}");
        start.elapsed()
    }

    /// 连续跑 `runs` 次，打印每次与汇总的 MB/s。
    async fn repeat(&self, what: &str, mut once: impl AsyncFnMut() -> Duration) {
        let mut rates = Vec::new();
        for i in 0..self.runs {
            let elapsed = once().await;
            let rate = self.bytes as f64 / (1 << 20) as f64 / elapsed.as_secs_f64();
            println!(
                "{what} 第 {} 次：{:.2} 秒，{rate:.1} MB/s",
                i + 1,
                elapsed.as_secs_f64()
            );
            rates.push(format!("{rate:.1}"));
        }
        println!("{what}（MB/s）：{}", rates.join(" / "));
    }

    /// 删掉下载的文件。
    fn clear_out(&self) {
        for entry in std::fs::read_dir(&self.out).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
    }
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// 本机的测试文件：随机内容（SSH 不压缩，内容不影响速度，但避免任何一层按零块优化）。
fn make_file(path: &Path, mb: u64) {
    let mut file = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    let mut block = vec![0u8; 1 << 20];
    for _ in 0..mb {
        getrandom::fill(&mut block).unwrap();
        file.write_all(&block).unwrap();
    }
    file.flush().unwrap();
}

/// 等到任务结束（大文件在慢的链路上可能要几分钟），返回它的状态。
async fn finish(m: &SftpManager, id: &TransferId) -> TransferView {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if let Some(v) = m
            .transfers(None)
            .into_iter()
            .find(|v| v.id == *id && v.state.is_finished())
        {
            return v;
        }
        assert!(Instant::now() < deadline, "等待传输结束超时");
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

fn scratch_dir() -> String {
    format!("/tmp/termlet-bench-{}", crate::id::random_token(6))
}

#[test]
#[ignore = "基准：连真实 sshd、传大文件，手动运行"]
fn sftp_throughput() {
    let Some(b) = Bench::new("sftp") else {
        return;
    };
    block_on(async {
        let m = manager();
        let s = m.open_ssh(b.target(None)).await.unwrap();
        let dir = scratch_dir();
        s.mkdir(&dir).await.unwrap();
        let remote = format!("{dir}/big.bin");
        b.repeat("上传（同一会话）", async || {
            b.upload(&m, &s, &dir).await
        })
        .await;
        b.repeat("下载（同一会话、自动改名）", async || {
            b.download(&m, &s, &remote).await
        })
        .await;
        b.clear_out();
        b.repeat("下载（每次新会话）", async || {
            let fresh = manager();
            let session = fresh.open_ssh(b.target(None)).await.unwrap();
            let elapsed = b.download(&fresh, &session, &remote).await;
            fresh.close_all().await;
            elapsed
        })
        .await;
        s.remove(&dir).await.unwrap();
        m.close_all().await;
    });
}

#[test]
#[ignore = "基准：连真实 sshd、传大文件，手动运行"]
fn sftp_shared_throughput() {
    let Some(b) = Bench::new("sftp-shared") else {
        return;
    };
    block_on(async {
        // 终端：连接组里的连接上开着 shell，输出照常读走
        let spec = TerminalSpec::for_host(&b.store, &b.host).unwrap();
        let group = ConnectionPool::new_group();
        let connection = ConnectionPool::global()
            .acquire(&group, || {
                connect(
                    b.store.clone(),
                    &spec.plan,
                    accept_host_key(),
                    &spec.connect,
                )
            })
            .await
            .unwrap();
        let io = open_shell(connection.clone(), ShellOptions::default())
            .await
            .unwrap();
        let output = io.output.clone();
        let drain = crate::runtime::spawn(async move { while output.recv().await.is_ok() {} });

        let m = manager();
        let s = m.open_ssh(b.target(Some(group))).await.unwrap();
        assert!(
            Arc::ptr_eq(&connection, &s.connection().unwrap()),
            "应共用终端的连接"
        );
        let dir = scratch_dir();
        s.mkdir(&dir).await.unwrap();
        let remote = format!("{dir}/big.bin");
        b.repeat("上传（共用终端的连接）", async || {
            b.upload(&m, &s, &dir).await
        })
        .await;
        b.repeat("下载（共用终端的连接）", async || {
            b.download(&m, &s, &remote).await
        })
        .await;
        s.remove(&dir).await.unwrap();
        m.close_all().await;
        io.input.close();
        drain.abort();
    });
}

#[test]
#[ignore = "基准：连真实 sshd、传大文件，手动运行"]
fn cli_throughput() {
    let Some(b) = Bench::new("cli") else {
        return;
    };
    let (address, port) = test_server().unwrap();
    block_on(async {
        let connection = connect_test(&address, port).await;
        let dir = scratch_dir();
        crate::ssh::exec(
            &connection,
            &format!("mkdir {dir}"),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let remote = format!("{dir}/big.bin");
        let local = b.out.join("big.bin");
        b.repeat("命令行上传（覆盖）", async || {
            let start = Instant::now();
            let done = crate::sftp::put_file(
                &connection,
                &b.local,
                &remote,
                false,
                std::future::pending(),
            )
            .await
            .unwrap();
            assert_eq!(done.bytes, b.bytes);
            start.elapsed()
        })
        .await;
        b.repeat("命令行下载（覆盖）", async || {
            let start = Instant::now();
            let done =
                crate::sftp::get_file(&connection, &remote, &local, false, std::future::pending())
                    .await
                    .unwrap();
            assert_eq!(done.bytes, b.bytes);
            start.elapsed()
        })
        .await;
        crate::ssh::exec(
            &connection,
            &format!("rm -r {dir}"),
            1024,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    });
}

#[test]
#[ignore = "基准：连真实 sshd、传大文件，手动运行"]
fn scp_throughput() {
    let Some(b) = Bench::new("scp") else {
        return;
    };
    let (address, port) = test_server().unwrap();
    block_on(async {
        let m = manager();
        let connection = connect_test(&address, port).await;
        let remote = ScpRemote::open(connection.clone(), crate::model::Encoding::Utf8)
            .await
            .unwrap();
        let link = Link {
            backend: Backend::Scp(Arc::new(remote)),
            connection: Some(connection),
            group: None,
        };
        let s = m.0.add("scp".into(), "w1".into(), link, None).await;
        let dir = scratch_dir();
        s.mkdir(&dir).await.unwrap();
        let remote = format!("{dir}/big.bin");
        b.repeat("SCP 上传（覆盖）", async || {
            b.upload(&m, &s, &dir).await
        })
        .await;
        b.repeat("SCP 下载（自动改名）", async || {
            b.download(&m, &s, &remote).await
        })
        .await;
        s.remove(&dir).await.unwrap();
        m.close_all().await;
    });
}
