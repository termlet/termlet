//! GitHub Gist：内存中的假 GitHub API（不用真实的账号与令牌）。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::dav::{Request, Response, Server, serve};
use super::*;
use crate::cloudsync::backend::{Backend, Fetch, Stored};
use crate::cloudsync::gist::{Gist, MANIFEST, PART_SIZE, part_name};

const TOKEN: &str = "ghp_termletFakeToken0123456789";

#[derive(Default, Clone)]
pub(crate) struct FakeGist {
    pub files: BTreeMap<String, String>,
    /// 提交的版本，最新的在前
    pub history: Vec<String>,
    pub public: bool,
    pub description: String,
}

/// 内存中的 GitHub API 的状态。
#[derive(Default)]
pub(crate) struct FakeGithub {
    pub gists: HashMap<String, FakeGist>,
    /// 经典令牌的权限（`X-OAuth-Scopes`）；`None` 为细粒度令牌（不给这个头）
    pub scopes: Option<String>,
    /// 读取时内容超过这么多字符的文件被截断（真实的 API 为 1 MB）
    pub truncate_over: usize,
    /// 速率限制已用完
    pub rate_limited: bool,
    pub patches: usize,
    pub raw_reads: usize,
    next: u32,
}

impl FakeGithub {
    fn new_id(&mut self) -> String {
        self.next += 1;
        format!("{:032x}", 0xabc000 + self.next)
    }

    /// 直接放一个 Gist（模拟别的电脑建的）。
    pub fn insert(&mut self, files: &[(&str, &str)]) -> String {
        let id = self.new_id();
        self.gists.insert(
            id.clone(),
            FakeGist {
                files: files
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                history: vec![format!("v{}", self.next)],
                ..Default::default()
            },
        );
        id
    }
}

fn gist_json(base: &str, id: &str, g: &FakeGist, truncate_over: usize) -> Value {
    let files: serde_json::Map<String, Value> = g
        .files
        .iter()
        .map(|(name, content)| {
            let truncated = content.len() > truncate_over;
            let shown = if truncated {
                &content[..truncate_over]
            } else {
                content.as_str()
            };
            (
                name.clone(),
                json!({
                    "filename": name,
                    "size": content.len(),
                    "truncated": truncated,
                    "content": shown,
                    "raw_url": format!("{base}/raw/{id}/{name}"),
                }),
            )
        })
        .collect();
    json!({
        "id": id,
        "public": g.public,
        "description": g.description,
        "files": files,
        "history": g.history.iter().map(|v| json!({ "version": v })).collect::<Vec<_>>(),
    })
}

fn json_response(status: u16, value: &Value) -> Response {
    Response::status(status)
        .header("Content-Type", "application/json")
        .body(value.to_string().as_bytes())
}

fn handle(r: &Request, s: &mut FakeGithub, base: &str) -> Response {
    // raw_url：不带令牌
    if let Some(rest) = r.path.strip_prefix("/raw/") {
        let (id, name) = rest.split_once('/').unwrap_or_default();
        s.raw_reads += 1;
        return match s.gists.get(id).and_then(|g| g.files.get(name)) {
            Some(content) => Response::status(200).body(content.as_bytes()),
            None => Response::status(404),
        };
    }
    if r.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
        return json_response(401, &json!({ "message": "Bad credentials" }));
    }
    if s.rate_limited {
        return Response::status(403)
            .header("x-ratelimit-remaining", "0")
            .body(b"{\"message\":\"API rate limit exceeded\"}");
    }
    let scopes = s.scopes.clone();
    let with_scopes = |response: Response| match &scopes {
        Some(scopes) => response.header("X-OAuth-Scopes", scopes),
        None => response,
    };
    let path = r.path.strip_prefix("/api/").unwrap_or_default();
    let (path, _query) = path.split_once('?').unwrap_or((path, ""));
    let truncate_over = s.truncate_over;
    match (r.method.as_str(), path) {
        ("GET", "gists") => {
            let list: Vec<Value> = s
                .gists
                .iter()
                .map(|(id, g)| {
                    let files: serde_json::Map<String, Value> = g
                        .files
                        .keys()
                        .map(|n| (n.clone(), json!({ "filename": n })))
                        .collect();
                    json!({ "id": id, "files": files })
                })
                .collect();
            with_scopes(json_response(200, &Value::Array(list)))
        }
        ("POST", "gists") => {
            let body: Value = serde_json::from_slice(&r.body).unwrap();
            let id = s.new_id();
            let files = body["files"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v["content"].as_str().unwrap().to_string()))
                .collect();
            let g = FakeGist {
                files,
                history: vec![format!("v{}", s.next)],
                public: body["public"].as_bool().unwrap(),
                description: body["description"].as_str().unwrap_or_default().into(),
            };
            let out = gist_json(base, &id, &g, truncate_over);
            s.gists.insert(id, g);
            json_response(201, &out)
        }
        (method, p) if p.starts_with("gists/") => {
            let id = p.trim_start_matches("gists/").to_string();
            if !s.gists.contains_key(&id) {
                return json_response(404, &json!({ "message": "Not Found" }));
            }
            if method == "PATCH" {
                if scopes.as_deref().is_some_and(|sc| !sc.contains("gist")) {
                    return json_response(403, &json!({ "message": "Forbidden" }));
                }
                let body: Value = serde_json::from_slice(&r.body).unwrap();
                s.next += 1;
                s.patches += 1;
                let version = format!("v{}", s.next);
                let g = s.gists.get_mut(&id).unwrap();
                for (name, file) in body["files"].as_object().unwrap() {
                    match file {
                        Value::Null => {
                            g.files.remove(name);
                        }
                        file => {
                            g.files.insert(
                                name.clone(),
                                file["content"].as_str().unwrap().to_string(),
                            );
                        }
                    }
                }
                g.history.insert(0, version);
            }
            let g = s.gists[&id].clone();
            with_scopes(json_response(200, &gist_json(base, &id, &g, truncate_over)))
        }
        _ => json_response(404, &json!({ "message": "Not Found" })),
    }
}

/// 启动假的 GitHub API；API 的地址为 `{base}/api/`。
pub(crate) fn github_server() -> (Arc<Mutex<FakeGithub>>, Server, String) {
    let state = Arc::new(Mutex::new(FakeGithub {
        scopes: Some("gist".into()),
        truncate_over: 1 << 20,
        ..Default::default()
    }));
    let shared = state.clone();
    let base = Arc::new(Mutex::new(String::new()));
    let base_in = base.clone();
    let server = serve(move |r| {
        let base = base_in.lock().unwrap().clone();
        handle(r, &mut shared.lock().unwrap(), &base)
    });
    *base.lock().unwrap() = server.base.clone();
    let api = format!("{}/api/", server.base);
    (state, server, api)
}

fn gist_input(gist_id: &str, token: &str, device: &str) -> SetupInput {
    SetupInput {
        target: SyncTarget::Gist {
            gist_id: gist_id.into(),
        },
        credential: SecretString::new(token),
        password: SecretString::new(PASSWORD),
        device_name: device.into(),
    }
}

fn gist_dev(api: &str) -> Dev {
    let dev = Dev::new();
    dev.sync.hooks().gist_api = Some(api.into());
    dev
}

fn configured_id(dev: &Dev) -> String {
    match dev.sync.status().config.unwrap().target {
        SyncTarget::Gist { gist_id } => gist_id,
        other => panic!("应是 Gist：{other:?}"),
    }
}

// 没有填写 Gist：测试连接时不新建；开始同步时新建私密 Gist 并记下 ID；另一台电脑只填令牌就找到它
#[test]
fn sync_over_gist() {
    let (gh, _server, api) = github_server();
    let (a, b) = (gist_dev(&api), gist_dev(&api));
    let h = a.add_host("web", "10.0.0.1", "pw");
    let info = block_on(a.sync.test_connection(&gist_input("", TOKEN, "a"))).unwrap();
    assert!(!info.exists);
    assert!(gh.lock().unwrap().gists.is_empty(), "测试连接不新建 Gist");
    block_on(a.sync.setup(gist_input("", TOKEN, "a"))).unwrap();
    let id = configured_id(&a);
    {
        let g = &gh.lock().unwrap().gists[&id];
        assert!(!g.public, "新建的是私密 Gist");
        assert!(g.description.contains("Termlet"));
        assert!(g.files.contains_key(MANIFEST));
    }
    a.sync();
    {
        let g = &gh.lock().unwrap().gists[&id];
        assert!(g.files.contains_key(&part_name(0)), "{:?}", g.files.keys());
        // 云端看不到主机名
        assert!(!g.files.values().any(|c| c.contains("10.0.0.1")));
    }
    let info = block_on(b.sync.setup(gist_input("", TOKEN, "b"))).unwrap();
    assert!(info.exists && info.counts.hosts == 1, "{info:?}");
    assert_eq!(configured_id(&b), id, "找到已有的 Gist");
    b.sync();
    assert_eq!(
        b.password(&h).as_deref(),
        Some("pw"),
        "经 Gist 同步后密码可用"
    );

    // 两台设备同时同步：写入前重新读取，发现其他设备先写入了，重新拉取合并
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

#[test]
fn gist_setup_errors() {
    let (gh, _server, api) = github_server();
    let dev = gist_dev(&api);
    let setup = |input: SetupInput| block_on(dev.sync.setup(input)).unwrap_err();
    assert_eq!(
        setup(gist_input("", "wrong-token", "a")),
        SyncError::AuthFailed
    );
    assert_eq!(
        setup(gist_input("", " ", "a")),
        SyncError::invalid("token", "required")
    );
    assert_eq!(
        setup(gist_input("0123456789abcdef0123", TOKEN, "a")),
        SyncError::invalid("gist", "not_found")
    );
    assert_eq!(
        setup(gist_input("not-a-gist!", TOKEN, "a")),
        SyncError::invalid("gist", "invalid")
    );
    // 经典令牌没有 gist 权限
    gh.lock().unwrap().scopes = Some("repo, read:user".into());
    assert_eq!(
        setup(gist_input("", TOKEN, "a")),
        SyncError::invalid("token", "no_gist_scope")
    );
    // 细粒度令牌（没有 X-OAuth-Scopes）照常
    gh.lock().unwrap().scopes = None;
    // 账号中有两个 Termlet 的 Gist：请用户指定
    let manifest = crate::cloudsync::gist::empty_manifest();
    let (one, two) = {
        let mut s = gh.lock().unwrap();
        s.insert(&[("notes.md", "别的 Gist")]);
        (
            s.insert(&[(MANIFEST, &manifest)]),
            s.insert(&[(MANIFEST, &manifest)]),
        )
    };
    match setup(gist_input("", TOKEN, "a")) {
        SyncError::GistAmbiguous { mut ids } => {
            ids.sort();
            assert_eq!(ids, [one.clone(), two.clone()]);
        }
        other => panic!("应请用户指定：{other:?}"),
    }
    // 指定一个（也可以填 Gist 的网址）
    let url = format!("https://gist.github.com/someone/{two}");
    block_on(dev.sync.setup(gist_input(&url, TOKEN, "a"))).unwrap();
    assert_eq!(configured_id(&dev), two);
    // 速率限制
    gh.lock().unwrap().rate_limited = true;
    assert_eq!(
        dev.try_run().unwrap_err(),
        SyncError::RateLimited { retry_after: None }
    );
}

// 被截断的文件按 raw_url 读取；段数变少时删掉多出来的段；放不下时不写入
#[test]
fn gist_parts_and_truncation() {
    let (gh, _server, api) = github_server();
    let id = gh.lock().unwrap().insert(&[
        (MANIFEST, &crate::cloudsync::gist::empty_manifest()),
        ("notes.md", "x"),
    ]);
    let g = Gist::new(&api, SecretString::new(TOKEN), &id).unwrap();
    block_on(g.check()).unwrap();
    assert!(matches!(block_on(g.get()).unwrap(), Fetch::Missing));

    let big: Vec<u8> = (0..PART_SIZE * 2).map(|i| (i % 253) as u8).collect();
    assert_eq!(block_on(g.put(&big, None)).unwrap(), Stored::Written);
    let parts = |gh: &Arc<Mutex<FakeGithub>>| {
        gh.lock().unwrap().gists[&id]
            .files
            .keys()
            .filter(|n| n.ends_with(".b64"))
            .count()
    };
    assert_eq!(parts(&gh), 3);
    gh.lock().unwrap().truncate_over = 1000;
    let Fetch::Found { data, etag } = block_on(g.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!(data, big);
    assert!(gh.lock().unwrap().raw_reads >= 3, "截断的段按 raw_url 读取");
    gh.lock().unwrap().truncate_over = 1 << 20;

    assert_eq!(
        block_on(g.put(b"small", Some("gist:stale"))).unwrap(),
        Stored::Changed
    );
    assert_eq!(block_on(g.put(b"small", None)).unwrap(), Stored::Changed);
    assert_eq!(
        block_on(g.put(b"small", Some(&etag))).unwrap(),
        Stored::Written
    );
    assert_eq!(parts(&gh), 1, "多出来的段已删除");
    assert!(
        gh.lock().unwrap().gists[&id].files.contains_key("notes.md"),
        "别的文件不动"
    );
    let Fetch::Found { data, .. } = block_on(g.get()).unwrap() else {
        panic!("应有同步文件")
    };
    assert_eq!(data, b"small");

    let patches = gh.lock().unwrap().patches;
    let huge = vec![0u8; crate::cloudsync::GIST_MAX_SIZE + 1];
    assert!(matches!(
        block_on(g.put(&huge, None)).unwrap_err(),
        SyncError::GistTooLarge { .. }
    ));
    assert_eq!(gh.lock().unwrap().patches, patches, "放不下时不发请求");

    // 段被改坏：不当作同步文件
    gh.lock()
        .unwrap()
        .gists
        .get_mut(&id)
        .unwrap()
        .files
        .insert(part_name(0), "AAAA".into());
    assert_eq!(block_on(g.get()).unwrap_err(), SyncError::Corrupt);
}
