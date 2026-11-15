//! 测试用：生成 minisign 签名的密钥、本机的最小 HTTP 服务器（不依赖外网）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use blake2::{Blake2b512, Digest as _};
use data_encoding::BASE64;
use ed25519_dalek::{Signer as _, SigningKey};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

/// 签名用的测试密钥（发布用的私钥只在发布者的离线介质中）。
pub(crate) struct TestKey {
    id: [u8; 8],
    signing: SigningKey,
}

impl TestKey {
    pub fn from_seed(seed: [u8; 32], id: [u8; 8]) -> Self {
        Self {
            id,
            signing: SigningKey::from_bytes(&seed),
        }
    }

    pub fn random() -> Self {
        let mut id = [0u8; 8];
        getrandom::fill(&mut id).unwrap();
        Self::with_id(id)
    }

    /// 指定密钥 ID 的随机密钥（冒充别的密钥 ID）。
    pub fn with_id(id: [u8; 8]) -> Self {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).unwrap();
        Self::from_seed(seed, id)
    }

    pub fn id(&self) -> [u8; 8] {
        self.id
    }

    /// minisign 格式的公钥（base64 的那一行）。
    pub fn public_key(&self) -> String {
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.id);
        raw.extend_from_slice(&self.signing.verifying_key().to_bytes());
        BASE64.encode(&raw)
    }

    pub fn sign(&self, message: &[u8], trusted_comment: &str) -> String {
        self.sign_with(
            message,
            trusted_comment,
            "signature from termlet release key",
        )
    }

    /// 签名，返回 minisign 格式的签名文件内容（与发布脚本的写法相同）。
    pub fn sign_with(&self, message: &[u8], trusted_comment: &str, untrusted: &str) -> String {
        let digest = Blake2b512::digest(message);
        let signature = self.signing.sign(&digest).to_bytes();
        let mut global = signature.to_vec();
        global.extend_from_slice(trusted_comment.as_bytes());
        let global = self.signing.sign(&global).to_bytes();
        let mut line = b"ED".to_vec();
        line.extend_from_slice(&self.id);
        line.extend_from_slice(&signature);
        format!(
            "untrusted comment: {untrusted}\n{}\ntrusted comment: {trusted_comment}\n{}\n",
            BASE64.encode(&line),
            BASE64.encode(&global)
        )
    }
}

/// 本机回环地址上的最小 HTTP 服务器：按路径返回预置的内容，其余 404；记录收到的请求（路径与查询）。
#[derive(Clone)]
pub(crate) struct TestServer {
    addr: SocketAddr,
    files: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl TestServer {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Self {
            addr: listener.local_addr().unwrap(),
            files: Arc::default(),
            requests: Arc::default(),
        };
        let state = server.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let state = state.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let target = head.split(' ').nth(1).unwrap_or("/").to_string();
                    state.requests.lock().unwrap().push(target.clone());
                    let path = target.split('?').next().unwrap_or("/").to_string();
                    let body = state.files.lock().unwrap().get(&path).cloned();
                    let response = match body {
                        Some(body) => {
                            let mut r = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            r.extend_from_slice(&body);
                            r
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = stream.write_all(&response).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        server
    }

    /// 地址，例如 `http://127.0.0.1:1234/feed`。
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn put(&self, path: &str, body: impl Into<Vec<u8>>) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_string(), body.into());
    }

    pub fn remove(&self, path: &str) {
        self.files.lock().unwrap().remove(path);
    }

    /// 收到的请求（路径与查询）。
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

/// 一个已关闭的本机端口（连接会被拒绝）。
pub(crate) async fn closed_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}
