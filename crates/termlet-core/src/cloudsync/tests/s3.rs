//! S3 兼容存储：内存中的假 S3 服务（按收到的请求重新计算签名核对），以及可选的真实服务（MinIO）集成测试。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use url::Url;

use super::dav::{Request, Response, Server, serve};
use super::*;
use crate::cloudsync::backend::{Backend, Fetch, Stored};
use crate::cloudsync::s3::{S3, normalize};
use crate::cloudsync::sigv4::{self, Signer};

const ACCESS_KEY: &str = "AKIDTERMLETTEST";
const SECRET_KEY: &str = "termlet/secret+key=example";
const BUCKET: &str = "termlet";

/// 内存中的 S3 服务的状态。
#[derive(Default)]
pub(crate) struct FakeS3 {
    pub buckets: HashSet<String>,
    /// 路径（`/桶/键`）→ 内容、版本
    pub objects: HashMap<String, (Vec<u8>, u32)>,
    /// 忽略 If-Match / If-None-Match（不支持条件写入、也不报错的服务）
    pub ignore_conditions: bool,
    /// 带条件写入时返回 501（明确不支持条件写入的服务）
    pub reject_conditions: bool,
    pub puts: usize,
    /// 签名核对失败的次数
    pub bad_signatures: usize,
}

fn error(status: u16, code: &str) -> Response {
    Response::status(status)
        .header("Content-Type", "application/xml")
        .body(
            format!(
                "<?xml version=\"1.0\"?><Error><Code>{code}</Code><Message>x</Message></Error>"
            )
            .as_bytes(),
        )
}

/// 按收到的请求重新计算签名（与客户端是否签了真正发出的内容核对）。
fn verify(r: &Request, body: &[u8]) -> Result<(), Response> {
    let Some(auth) = r.header("authorization") else {
        return Err(error(403, "AccessDenied"));
    };
    let part = |name: &str| {
        auth.split([' ', ','])
            .find_map(|p| p.trim().strip_prefix(name))
            .unwrap_or_default()
            .to_string()
    };
    let credential = part("Credential=");
    let signed = part("SignedHeaders=");
    let mut scope = credential.split('/');
    let (key, _date, region) = (
        scope.next().unwrap_or_default(),
        scope.next().unwrap_or_default(),
        scope.next().unwrap_or_default(),
    );
    if key != ACCESS_KEY {
        return Err(error(403, "InvalidAccessKeyId"));
    }
    let payload = r.header("x-amz-content-sha256").unwrap_or_default();
    if payload != sigv4::sha256_hex(body) {
        return Err(error(400, "XAmzContentSHA256Mismatch"));
    }
    let host = r.header("host").unwrap_or_default();
    let url = Url::parse(&format!("http://{host}{}", r.path)).unwrap();
    let names: Vec<&str> = signed.split(';').collect();
    let values: Vec<(String, String)> = names
        .iter()
        .map(|n| (n.to_string(), r.header(n).unwrap_or_default().to_string()))
        .collect();
    let headers: Vec<(&str, &str)> = values
        .iter()
        .map(|(n, v)| (n.as_str(), v.as_str()))
        .collect();
    let signer = Signer {
        access_key: ACCESS_KEY.into(),
        secret: SecretString::new(SECRET_KEY),
        region: region.into(),
        service: "s3".into(),
    };
    let date = r.header("x-amz-date").unwrap_or_default();
    let expected = signer.authorization(
        &sigv4::Request {
            method: &r.method,
            url: &url,
            headers: &headers,
            payload_sha256: payload,
        },
        date,
    );
    if expected != auth || !names.contains(&"host") || !names.contains(&"x-amz-date") {
        return Err(error(403, "SignatureDoesNotMatch"));
    }
    Ok(())
}

fn etag(rev: u32) -> String {
    format!("\"e{rev}\"")
}

/// 启动内存中的 S3 服务（已有桶 `termlet`）。
pub(crate) fn s3_server() -> (Arc<Mutex<FakeS3>>, Server) {
    let state = Arc::new(Mutex::new(FakeS3 {
        buckets: [BUCKET.to_string()].into(),
        ..Default::default()
    }));
    let shared = state.clone();
    let server = serve(move |r| {
        let mut s = shared.lock().unwrap();
        if let Err(response) = verify(r, &r.body) {
            s.bad_signatures += 1;
            return response;
        }
        let path = r.path.split('?').next().unwrap_or_default().to_string();
        let bucket = path
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or_default();
        if !s.buckets.contains(bucket) {
            return error(404, "NoSuchBucket");
        }
        let current = s.objects.get(&path).map(|(_, rev)| *rev);
        match r.method.as_str() {
            "GET" | "HEAD" => match s.objects.get(&path) {
                Some((data, rev)) => {
                    let body = match (r.method.as_str(), r.header("range")) {
                        ("HEAD", _) => Vec::new(),
                        (_, Some(_)) => data[..data.len().min(1)].to_vec(),
                        _ => data.clone(),
                    };
                    let status = if r.header("range").is_some() {
                        206
                    } else {
                        200
                    };
                    Response::status(status)
                        .header("ETag", &etag(*rev))
                        .body(&body)
                }
                None if r.method == "HEAD" => Response::status(404),
                None => error(404, "NoSuchKey"),
            },
            "PUT" => {
                let conditional =
                    r.header("if-match").is_some() || r.header("if-none-match").is_some();
                if conditional && s.reject_conditions {
                    return error(501, "NotImplemented");
                }
                if !s.ignore_conditions {
                    if r.header("if-none-match") == Some("*") && current.is_some() {
                        return error(412, "PreconditionFailed");
                    }
                    if let Some(tag) = r.header("if-match")
                        && current.map(etag).as_deref() != Some(tag)
                    {
                        return error(412, "PreconditionFailed");
                    }
                }
                let rev = current.unwrap_or(0) + 1;
                s.objects.insert(path, (r.body.clone(), rev));
                s.puts += 1;
                Response::status(200).header("ETag", &etag(rev))
            }
            "DELETE" => {
                s.objects.remove(&path);
                Response::status(204)
            }
            _ => error(405, "MethodNotAllowed"),
        }
    });
    (state, server)
}

fn s3_target(base: &str, prefix: &str) -> SyncTarget {
    SyncTarget::S3 {
        endpoint: base.into(),
        region: String::new(),
        bucket: BUCKET.into(),
        prefix: prefix.into(),
        access_key_id: ACCESS_KEY.into(),
        path_style: true,
        allow_http: true,
    }
}

fn s3_input(base: &str, device: &str) -> SetupInput {
    SetupInput {
        target: s3_target(base, "team/termlet"),
        credential: SecretString::new(SECRET_KEY),
        password: SecretString::new(PASSWORD),
        device_name: device.into(),
    }
}

fn backend(base: &str, secret: &str) -> S3 {
    let l = normalize(base, "", BUCKET, "p", ACCESS_KEY, true, true).unwrap();
    S3::new(&l, SecretString::new(secret)).unwrap()
}

const OBJECT: &str = "/termlet/p/termlet-sync.tlsync";

#[test]
fn s3_backend() {
    let (s3, server) = s3_server();
    let b = backend(&server.base, SECRET_KEY);
    // 检查：读、写入并删除探测对象
    block_on(b.check()).unwrap();
    assert!(s3.lock().unwrap().objects.is_empty(), "探测对象已删除");
    assert_eq!(s3.lock().unwrap().puts, 1);
    assert!(matches!(block_on(b.get()).unwrap(), Fetch::Missing));
    assert_eq!(block_on(b.put(b"v1", None)).unwrap(), Stored::Written);
    assert_eq!(
        block_on(b.put(b"v1", None)).unwrap(),
        Stored::Changed,
        "已存在时新建"
    );
    let Fetch::Found { data, etag } = block_on(b.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!((data.as_slice(), etag.as_str()), (&b"v1"[..], "\"e1\""));
    assert_eq!(
        block_on(b.put(b"v2", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(
        block_on(b.put(b"v3", Some(&etag))).unwrap(),
        Stored::Changed,
        "旧的 ETag"
    );
    assert_eq!(s3.lock().unwrap().objects[OBJECT].0, b"v2");
    // 只靠条件写入（跳过写入前的比较）：服务按 If-Match / If-None-Match 拒绝
    let mut only_conditions = backend(&server.base, SECRET_KEY);
    only_conditions.skip_precheck = true;
    assert_eq!(
        block_on(only_conditions.put(b"v3", Some(&etag))).unwrap(),
        Stored::Changed
    );
    assert_eq!(
        block_on(only_conditions.put(b"v3", None)).unwrap(),
        Stored::Changed
    );
    assert_eq!(s3.lock().unwrap().bad_signatures, 0, "每个请求的签名都对");

    // 秘密访问密钥不对、桶不存在
    let wrong = backend(&server.base, "wrong-secret");
    assert_eq!(block_on(wrong.get()).unwrap_err(), SyncError::AuthFailed);
    assert_eq!(block_on(wrong.check()).unwrap_err(), SyncError::AuthFailed);
    let l = normalize(&server.base, "", "nope", "", ACCESS_KEY, true, true).unwrap();
    let missing = S3::new(&l, SecretString::new(SECRET_KEY)).unwrap();
    assert_eq!(
        block_on(missing.check()).unwrap_err(),
        SyncError::invalid("bucket", "not_found")
    );
}

// 不支持条件写入的服务：静默忽略时写入前的比较仍然发现其他设备的写入；返回 501 时不带条件重写
#[test]
fn s3_without_conditional_writes() {
    let (s3, server) = s3_server();
    let b = backend(&server.base, SECRET_KEY);
    s3.lock().unwrap().ignore_conditions = true;
    assert_eq!(block_on(b.put(b"v1", None)).unwrap(), Stored::Written);
    let Fetch::Found { etag, .. } = block_on(b.get()).unwrap() else {
        panic!("应有同步文件")
    };
    // 其他设备在这之后写入了
    s3.lock()
        .unwrap()
        .objects
        .insert(OBJECT.into(), (b"other".to_vec(), 7));
    assert_eq!(
        block_on(b.put(b"v2", Some(&etag))).unwrap(),
        Stored::Changed
    );
    assert_eq!(block_on(b.put(b"v2", None)).unwrap(), Stored::Changed);
    assert_eq!(s3.lock().unwrap().objects[OBJECT].0, b"other", "没有覆盖");

    let mut state = s3.lock().unwrap();
    state.ignore_conditions = false;
    state.reject_conditions = true;
    drop(state);
    assert_eq!(
        block_on(b.put(b"v3", Some("\"e7\""))).unwrap(),
        Stored::Written,
        "501 时不带条件重写"
    );
    assert_eq!(s3.lock().unwrap().objects[OBJECT].0, b"v3");
}

#[test]
fn sync_over_s3() {
    let (s3, server) = s3_server();
    let (a, b) = (Dev::new(), Dev::new());
    let h = a.add_host("web", "10.0.0.1", "pw");
    let mut insecure = s3_input(&server.base, "a");
    if let SyncTarget::S3 { allow_http, .. } = &mut insecure.target {
        *allow_http = false;
    }
    assert_eq!(
        block_on(a.sync.setup(insecure)).unwrap_err(),
        SyncError::invalid("endpoint", "insecure"),
        "没有勾选允许 HTTP"
    );
    let mut wrong = s3_input(&server.base, "a");
    wrong.credential = SecretString::new("wrong");
    assert_eq!(
        block_on(a.sync.setup(wrong)).unwrap_err(),
        SyncError::AuthFailed
    );
    // 上面密钥不对的那次签名当然对不上；之后的都应对上
    s3.lock().unwrap().bad_signatures = 0;
    block_on(a.sync.setup(s3_input(&server.base, "a"))).unwrap();
    a.sync();
    assert!(
        s3.lock()
            .unwrap()
            .objects
            .contains_key("/termlet/team/termlet/termlet-sync.tlsync")
    );
    let info = block_on(b.sync.setup(s3_input(&server.base, "b"))).unwrap();
    assert!(info.exists && info.counts.hosts == 1, "{info:?}");
    b.sync();
    assert_eq!(
        b.password(&h).as_deref(),
        Some("pw"),
        "经 S3 同步后密码可用"
    );
    let SyncTarget::S3 {
        endpoint, region, ..
    } = b.sync.status().config.unwrap().target
    else {
        panic!("应是 S3")
    };
    assert_eq!(
        (endpoint.as_str(), region.as_str()),
        (format!("{}/", server.base).as_str(), "us-east-1")
    );

    // 两台设备同时同步：后写入的被 If-Match 拒绝，重新拉取合并
    a.add_host("a2", "10.0.0.2", "p");
    b.add_host("b2", "10.0.0.3", "p");
    let bs = b.sync.clone();
    race_before_put(
        &a.sync,
        Box::pin(async move {
            bs.sync_now().await.unwrap();
        }),
    );
    a.sync();
    b.sync();
    assert_eq!(a.names(), ["a2", "b2", "web"]);
    assert_eq!(b.names(), ["a2", "b2", "web"]);
    assert_eq!(s3.lock().unwrap().bad_signatures, 0);
}

// 真实的 S3 兼容服务（可选）：设置 TERMLET_TEST_S3（终端节点）、TERMLET_TEST_S3_BUCKET（已建好的桶）、
// TERMLET_TEST_S3_KEY、TERMLET_TEST_S3_SECRET 时运行，否则跳过。例如用 Docker 起 MinIO：
// `docker run -d --name termlet-sync-minio -p 127.0.0.1:19000:9000 -e MINIO_ROOT_USER=termlet
// -e MINIO_ROOT_PASSWORD=termlet-minio-pass minio/minio server /data`，再用 mc 建桶
// （`docker run --rm --network container:termlet-sync-minio --entrypoint sh minio/mc -c "mc alias set m
// http://127.0.0.1:9000 termlet termlet-minio-pass && mc mb m/termlet-test"`），地址为 `http://127.0.0.1:19000`，
// 用完 `docker rm -f termlet-sync-minio`。
#[test]
fn real_s3_server() {
    let Ok(endpoint) = std::env::var("TERMLET_TEST_S3") else {
        eprintln!("没有设置 TERMLET_TEST_S3，跳过");
        return;
    };
    let bucket = std::env::var("TERMLET_TEST_S3_BUCKET").unwrap_or_else(|_| "termlet-test".into());
    let key = std::env::var("TERMLET_TEST_S3_KEY").unwrap_or_default();
    let secret = std::env::var("TERMLET_TEST_S3_SECRET").unwrap_or_default();
    let region = std::env::var("TERMLET_TEST_S3_REGION").unwrap_or_default();
    let prefix = format!("termlet-test-{}", crate::id::random_token(5));
    let target = SyncTarget::S3 {
        endpoint: endpoint.clone(),
        region: region.clone(),
        bucket: bucket.clone(),
        prefix: prefix.clone(),
        access_key_id: key.clone(),
        path_style: true,
        allow_http: true,
    };
    let input = |device: &str| SetupInput {
        target: target.clone(),
        credential: SecretString::new(secret.as_str()),
        password: SecretString::new(PASSWORD),
        device_name: device.into(),
    };
    let (a, b) = (Dev::new(), Dev::new());
    let h = a.add_host("web", "10.0.0.1", "pw");
    block_on(a.sync.setup(input("a"))).unwrap();
    a.sync();
    let info = block_on(b.sync.setup(input("b"))).unwrap();
    assert!(info.exists && info.counts.hosts == 1);
    b.sync();
    assert_eq!(b.password(&h).as_deref(), Some("pw"));
    b.rename_host(&h, "web-b");
    b.sync();
    a.sync();
    assert_eq!(a.names(), ["web-b"]);
    // 两台设备同时同步：服务按 If-Match 拒绝后来的写入（412），重新拉取合并
    a.add_host("a2", "10.0.0.2", "p");
    b.add_host("b2", "10.0.0.3", "p");
    let bs = b.sync.clone();
    race_before_put(
        &a.sync,
        Box::pin(async move {
            bs.sync_now().await.unwrap();
        }),
    );
    a.sync();
    a.sync.hooks().wrap = None;
    b.sync();
    assert_eq!(a.names(), ["a2", "b2", "web-b"]);
    assert_eq!(b.names(), ["a2", "b2", "web-b"]);
    // 条件写入：旧的 ETag、新建时已存在
    let l = normalize(&endpoint, &region, &bucket, &prefix, &key, true, true).unwrap();
    let s3 = S3::new(&l, SecretString::new(secret.as_str())).unwrap();
    let Fetch::Found { etag, .. } = block_on(s3.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!(block_on(s3.put(b"v1", None)).unwrap(), Stored::Changed);
    assert_eq!(
        block_on(s3.put(b"v2", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(
        block_on(s3.put(b"v3", Some(&etag))).unwrap(),
        Stored::Changed
    );
    // 服务自己按条件拒绝（跳过写入前的比较）
    let mut s3 = s3;
    s3.skip_precheck = true;
    assert_eq!(
        block_on(s3.put(b"v4", Some(&etag))).unwrap(),
        Stored::Changed,
        "If-Match"
    );
    assert_eq!(
        block_on(s3.put(b"v4", None)).unwrap(),
        Stored::Changed,
        "If-None-Match"
    );
    // 密钥不对、桶不存在
    let mut wrong = input("c");
    wrong.credential = SecretString::new("wrong-secret-key");
    assert_eq!(
        block_on(Dev::new().sync.setup(wrong)).unwrap_err(),
        SyncError::AuthFailed
    );
    let mut nobucket = input("c");
    if let SyncTarget::S3 { bucket, .. } = &mut nobucket.target {
        *bucket = format!("termlet-missing-{}", crate::id::random_token(4));
    }
    assert_eq!(
        block_on(Dev::new().sync.setup(nobucket)).unwrap_err(),
        SyncError::invalid("bucket", "not_found")
    );
}
