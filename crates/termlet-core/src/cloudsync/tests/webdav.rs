//! WebDAV，以及可选的真实 WebDAV 服务器集成测试。

use std::sync::atomic::{AtomicUsize, Ordering};

use super::dav::{DAV_PASSWORD, DAV_USER, Response, dav_server, serve};
use super::*;
use crate::cloudsync::backend::{Backend, Fetch, Stored};
use crate::cloudsync::webdav::{WebDav, parse_dir};

fn client(url: &str, password: &str) -> WebDav {
    WebDav::new(url, true, DAV_USER, SecretString::new(password)).unwrap()
}

fn webdav_input(url: &str, password: &str) -> SetupInput {
    SetupInput {
        target: SyncTarget::WebDav {
            url: url.into(),
            username: DAV_USER.into(),
            allow_http: true,
        },
        credential: SecretString::new(password),
        password: SecretString::new(PASSWORD),
        device_name: "dev".into(),
    }
}

#[test]
fn webdav_backend() {
    let (dav, server) = dav_server();
    let url = format!("{}/dav/termlet", server.base);
    assert_eq!(
        block_on(client(&url, "wrong").check()).unwrap_err(),
        SyncError::AuthFailed,
        "账号错误"
    );
    assert_eq!(
        parse_dir("ftp://x", true).unwrap_err(),
        SyncError::invalid("url", "invalid")
    );
    assert_eq!(
        parse_dir("https://", true).unwrap_err(),
        SyncError::invalid("url", "invalid")
    );
    assert_eq!(
        parse_dir("https://me:pw@dav.example.com/dav", true).unwrap_err(),
        SyncError::invalid("url", "has_credentials")
    );
    assert_eq!(
        parse_dir(" https://dav.example.com/a/b ", false)
            .unwrap()
            .as_str(),
        "https://dav.example.com/a/b/"
    );
    // http:// 要明确允许
    assert_eq!(
        parse_dir("http://nas.local/dav", false).unwrap_err(),
        SyncError::invalid("url", "insecure")
    );

    // 目录不存在时 check 建立它
    let wd = client(&url, DAV_PASSWORD);
    block_on(wd.check()).unwrap();
    assert!(dav.lock().unwrap().dirs.contains("/dav/termlet/"));
    assert!(matches!(block_on(wd.get()).unwrap(), Fetch::Missing));
    assert_eq!(block_on(wd.put(b"v1", None)).unwrap(), Stored::Written);
    assert_eq!(
        block_on(wd.put(b"v1", None)).unwrap(),
        Stored::Changed,
        "已存在时新建"
    );
    let Fetch::Found { etag, .. } = block_on(wd.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!(etag, "\"r1\"");
    assert_eq!(
        block_on(wd.put(b"v2", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(
        block_on(wd.put(b"v3", Some(&etag))).unwrap(),
        Stored::Changed,
        "旧的 ETag"
    );

    // 只有弱 ETag：比较内容
    dav.lock().unwrap().weak = true;
    let Fetch::Found { data, etag } = block_on(wd.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert!(etag.starts_with("sha256:") && data == b"v2", "{etag}");
    assert_eq!(
        block_on(wd.put(b"v3", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(
        block_on(wd.put(b"v4", Some(&etag))).unwrap(),
        Stored::Changed,
        "内容已变"
    );

    // 目录被删除后写入：先建立目录
    {
        let mut d = dav.lock().unwrap();
        d.dirs.remove("/dav/termlet/");
        d.files.clear();
    }
    let slash = client(&format!("{}/dav/termlet/", server.base), DAV_PASSWORD);
    assert_eq!(
        block_on(slash.put(b"x", None)).unwrap(),
        Stored::Written,
        "目录不存在时写入"
    );
}

#[test]
fn sync_over_webdav() {
    let (_dav, server) = dav_server();
    let url = format!("{}/dav/termlet/", server.base);
    let (a, b) = (Dev::new(), Dev::new());
    let h = a.add_host("web", "10.0.0.1", "pw");
    let mut insecure = webdav_input(&url, DAV_PASSWORD);
    if let SyncTarget::WebDav { allow_http, .. } = &mut insecure.target {
        *allow_http = false;
    }
    assert_eq!(
        block_on(a.sync.setup(insecure)).unwrap_err(),
        SyncError::invalid("url", "insecure"),
        "没有勾选允许 HTTP"
    );
    assert_eq!(
        block_on(a.sync.setup(webdav_input(&url, "wrong"))).unwrap_err(),
        SyncError::AuthFailed,
        "WebDAV 密码错误"
    );
    block_on(a.sync.setup(webdav_input(&url, DAV_PASSWORD))).unwrap();
    a.sync();
    let info = block_on(b.sync.setup(webdav_input(&url, DAV_PASSWORD))).unwrap();
    assert!(info.exists && info.counts.hosts == 1, "{info:?}");
    b.sync();
    assert_eq!(
        b.password(&h).as_deref(),
        Some("pw"),
        "经 WebDAV 同步后密码可用"
    );
    let config = b.sync.status().config.unwrap();
    assert_eq!(
        config.target,
        SyncTarget::WebDav {
            url: url.clone(),
            username: DAV_USER.into(),
            allow_http: true,
        }
    );
    // 两台设备都改了：其他设备先写入时 412，重新拉取合并
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
}

// 写入被重定向（例如 http 跳转到 https）不能当作写入成功；读取可以跟随跳转
#[test]
fn webdav_redirect_is_error() {
    let puts = std::sync::Arc::new(AtomicUsize::new(0));
    let counter = puts.clone();
    let server = serve(move |r| {
        if let Some(rest) = r.path.strip_prefix("/moved/") {
            if r.method == "PUT" {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            if rest == FILE_NAME && r.method == "GET" {
                return Response::status(200).body(b"data");
            }
            return Response::status(200);
        }
        let target = format!("/moved/{}", r.path.trim_start_matches("/dav/"));
        Response::status(301).header("Location", &target)
    });
    let wd = client(&format!("{}/dav/", server.base), "p");
    match block_on(wd.put(b"x", None)).unwrap_err() {
        SyncError::Redirected { status, location } => {
            assert_eq!(status, 301);
            assert!(location.contains("/moved/"), "{location}");
        }
        other => panic!("写入被重定向应报错并给出跳转后的地址：{other:?}"),
    }
    assert_eq!(puts.load(Ordering::SeqCst), 0);
    assert!(
        matches!(
            block_on(wd.check()).unwrap_err(),
            SyncError::Redirected { .. }
        ),
        "检查地址时被重定向"
    );
    let Fetch::Found { data, .. } = block_on(wd.get()).unwrap() else {
        panic!("读取应跟随跳转")
    };
    assert_eq!(data, b"data");
}

// 服务器的错误码与连不上：空间不足、其他错误、拒绝连接
#[test]
fn webdav_errors_are_classified() {
    let server = serve(|r| match r.method.as_str() {
        "PUT" => Response::status(507),
        "PROPFIND" => Response::status(500),
        _ => Response::status(404),
    });
    let wd = client(&format!("{}/dav/", server.base), "p");
    assert_eq!(
        block_on(wd.put(b"x", None)).unwrap_err(),
        SyncError::InsufficientStorage
    );
    assert_eq!(
        block_on(wd.check()).unwrap_err(),
        SyncError::Server { status: 500 }
    );
    assert!(matches!(block_on(wd.get()).unwrap(), Fetch::Missing));
    // 没有在监听的端口
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let wd = client(&format!("http://127.0.0.1:{closed}/dav/"), "p");
    let result = block_on(wd.get());
    assert!(
        matches!(result, Err(SyncError::Network { .. })),
        "{:?}",
        result.err()
    );
}

// 真实的 WebDAV 服务器（可选）：设置 TERMLET_TEST_WEBDAV（目录地址）、TERMLET_TEST_WEBDAV_USER、
// TERMLET_TEST_WEBDAV_PASSWORD 时运行，否则跳过。例如用 Docker 起 Apache mod_dav：
// `docker run -d --name termlet-webdav-test -p 127.0.0.1:18089:80 -e AUTH_TYPE=Basic -e USERNAME=tester
// -e PASSWORD=dav-pass-123 bytemark/webdav`，地址为 `http://127.0.0.1:18089/`，用完 `docker rm -f termlet-webdav-test`。
#[test]
fn real_webdav_server() {
    let Ok(base) = std::env::var("TERMLET_TEST_WEBDAV") else {
        eprintln!("没有设置 TERMLET_TEST_WEBDAV，跳过");
        return;
    };
    let user = std::env::var("TERMLET_TEST_WEBDAV_USER").unwrap_or_default();
    let pass = std::env::var("TERMLET_TEST_WEBDAV_PASSWORD").unwrap_or_default();
    let url = format!(
        "{}/termlet-test-{}/",
        base.trim_end_matches('/'),
        crate::id::random_token(5)
    );
    let input = |device: &str| SetupInput {
        target: SyncTarget::WebDav {
            url: url.clone(),
            username: user.clone(),
            allow_http: true,
        },
        credential: SecretString::new(pass.as_str()),
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
    // 两台设备同时同步：服务器按 If-Match 拒绝后来的写入（412），重新拉取合并
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
    // 旧的版本标记、新建时已存在
    let wd = WebDav::new(&url, true, &user, SecretString::new(pass.as_str())).unwrap();
    // （Apache 在文件刚修改后的一秒内只给弱 ETag，这时退回为比较内容：每次写入的内容不同，真实同步中密文每次都不同）
    let Fetch::Found { etag, .. } = block_on(wd.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!(block_on(wd.put(b"v1", None)).unwrap(), Stored::Changed);
    assert_eq!(
        block_on(wd.put(b"v2", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(
        block_on(wd.put(b"v3", Some(&etag))).unwrap(),
        Stored::Changed
    );
    let mut wrong = input("c");
    wrong.credential = SecretString::new("wrong-password");
    assert_eq!(
        block_on(Dev::new().sync.setup(wrong)).unwrap_err(),
        SyncError::AuthFailed
    );
}

// 旧版本保存的 WebDAV 设置没有 allowHttp：当作已允许（升级前在用 HTTP 的继续能同步）
#[test]
fn legacy_webdav_config_keeps_http() {
    let old = r#"{"type":"webdav","url":"http://nas.local/dav/","username":"me"}"#;
    let target: SyncTarget = serde_json::from_str(old).unwrap();
    assert_eq!(
        target,
        SyncTarget::WebDav {
            url: "http://nas.local/dav/".into(),
            username: "me".into(),
            allow_http: true,
        }
    );
    let json = serde_json::to_value(&target).unwrap();
    assert_eq!(json["allowHttp"], true);
}
