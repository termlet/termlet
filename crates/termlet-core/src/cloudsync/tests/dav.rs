//! 测试用的最小 HTTP/1.1 服务器：每个连接处理一个请求后关闭，请求交给处理函数。WebDAV 的测试用它模拟服务器，
//! 不依赖外网。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::testkit::block_on;

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Basic 认证的用户名与密码。
    pub fn basic_auth(&self) -> Option<(String, String)> {
        let encoded = self.header("authorization")?.strip_prefix("Basic ")?;
        let decoded = data_encoding::BASE64.decode(encoded.as_bytes()).ok()?;
        let text = String::from_utf8(decoded).ok()?;
        let (user, pass) = text.split_once(':')?;
        Some((user.to_string(), pass.to_string()))
    }
}

pub(crate) struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: &[u8]) -> Self {
        self.body = body.to_vec();
        self
    }
}

pub(crate) struct Server {
    /// `http://127.0.0.1:端口`
    pub base: String,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Handler = Arc<dyn Fn(&Request) -> Response + Send + Sync>;

pub(crate) fn serve(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Server {
    let listener = block_on(TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handler: Handler = Arc::new(handler);
    let task = crate::runtime::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let handler = handler.clone();
            tokio::spawn(async move {
                let _ = handle(stream, handler).await;
            });
        }
    });
    Server { base, task }
}

async fn handle(mut stream: TcpStream, handler: Handler) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let head_end = loop {
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_string();
    let path = first.next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            let (k, v) = line.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < length {
        let mut chunk = vec![0u8; length - body.len()];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let request = Request {
        method,
        path,
        headers,
        body,
    };
    let response = handler(&request);
    let mut out = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (k, v) in &response.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    stream.write_all(out.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

/// 内存中的 WebDAV 服务器的状态：Basic 认证、PROPFIND、MKCOL、GET、
/// PUT（If-Match / If-None-Match）。
#[derive(Default)]
pub(crate) struct Dav {
    pub dirs: HashSet<String>,
    pub files: HashMap<String, Vec<u8>>,
    rev: HashMap<String, u32>,
    /// 只返回弱 ETag（客户端应退回为比较内容）
    pub weak: bool,
    pub puts: usize,
}

impl Dav {
    fn etag(&self, path: &str) -> String {
        let tag = format!("\"r{}\"", self.rev.get(path).copied().unwrap_or(0));
        if self.weak { format!("W/{tag}") } else { tag }
    }
}

pub(crate) const DAV_USER: &str = "me";
pub(crate) const DAV_PASSWORD: &str = "app-pass";

/// 启动内存中的 WebDAV 服务器（已有目录 `/dav/`）。
pub(crate) fn dav_server() -> (Arc<Mutex<Dav>>, Server) {
    let state = Arc::new(Mutex::new(Dav {
        dirs: ["/dav/".to_string()].into(),
        ..Default::default()
    }));
    let shared = state.clone();
    let server = serve(move |r| {
        if r.basic_auth() != Some((DAV_USER.into(), DAV_PASSWORD.into())) {
            return Response::status(401);
        }
        let mut d = shared.lock().unwrap();
        let p = r.path.clone();
        match r.method.as_str() {
            "PROPFIND" if d.dirs.contains(&p) => Response::status(207),
            "PROPFIND" => Response::status(404),
            "MKCOL" if d.dirs.contains(&p) => Response::status(405),
            "MKCOL" => {
                d.dirs.insert(p);
                Response::status(201)
            }
            "GET" => match d.files.get(&p) {
                Some(data) => Response::status(200).header("ETag", &d.etag(&p)).body(data),
                None => Response::status(404),
            },
            "PUT" => {
                let dir = p[..=p.rfind('/').unwrap_or(0)].to_string();
                if !d.dirs.contains(&dir) {
                    return Response::status(409);
                }
                let exists = d.files.contains_key(&p);
                if r.header("if-none-match") == Some("*") && exists {
                    return Response::status(412);
                }
                if let Some(m) = r.header("if-match")
                    && (!exists || m != d.etag(&p))
                {
                    return Response::status(412);
                }
                d.files.insert(p.clone(), r.body.clone());
                *d.rev.entry(p).or_default() += 1;
                d.puts += 1;
                Response::status(201)
            }
            _ => Response::status(405),
        }
    });
    (state, server)
}
