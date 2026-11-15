//! Tabby（原 Terminus）的配置文件 config.yaml 中的 SSH 配置（profile）。
//!
//! 与导入相关的部分：
//!
//! ```yaml
//! profiles:
//!   - id: ssh:custom:web:7c1e…        # 跳板机按 id 引用（旧版本按名称引用）
//!     type: ssh                      # 只导入 ssh，local、serial、telnet 等跳过
//!     name: web
//!     group: 5f0b…                   # 分组 id（配置版本 5 之前直接是分组名）
//!     options:
//!       host: 10.0.0.1
//!       port: 22                     # 与默认值相同的选项不会写入配置，按 Tabby 的规则补全
//!       user: root
//!       privateKeys: [file://C:\Users\me\.ssh\id_ed25519]
//!       auth: agent                  # 不写为自动；password、publicKey、agent、keyboardInteractive
//!       jumpHost: ssh:custom:bastion:…
//! groups:
//!   - {id: 5f0b…, name: 生产, parentGroupId: …, defaults: {ssh: {options: {user: deploy}}}}
//! profileDefaults:
//!   ssh: {options: {user: admin}}
//! ```
//!
//! 选项按 Tabby 的规则补全：配置自身 → 所在分组的默认值 → 全局 SSH 默认值 → 内置默认值（端口 22、用户 root）。
//! 分组按 id 解析为各级名称（含上级分组）；私钥取第一个（`file://` 开头的为本机文件）；认证方式为 agent 时
//! 使用 ssh-agent；跳板机引用本配置中的其他 SSH 配置时导入为跳板机。不导入密码。
//! 以上规则对照过 Tabby 1.0.238 的源码（ProfilesService.getProfileDefaults、tabby-ssh 的 configDefaults），
//! `testdata/tabby/real-1.0.238.yaml` 是在该版本的界面中实际创建的配置。
//!
//! 个别字段的类型不符时忽略该字段、照常解析其余部分；只有 YAML 语法错误才放弃。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{Auth, Host, KeySource, Proxy, ProxyKind};

use super::files::{LocalEnv, decode_text, read_config_file};
use super::plan::{Endpoint, Hint, JumpRef, PlanBuilder, PlannedHost};
use super::{ImportError, ImportPlan, Source};

/// `path` 也可以是配置目录（便携版为程序目录下的 data 目录）。
pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let (raw, _) = read_config_file(path, "config.yaml", env)?;
    parse(&decode_text(&raw, env.ansi), env)
}

/// Tabby 的配置位于 Electron 的 userData 目录（Windows 为 `%APPDATA%\tabby`，macOS 为
/// `~/Library/Application Support/tabby`，Linux 为 `~/.config/tabby`）；旧版名为 Terminus。
pub(super) fn default_candidates(env: &LocalEnv) -> Vec<PathBuf> {
    let Some(dir) = &env.config_dir else {
        return Vec::new();
    };
    vec![
        dir.join("tabby").join("config.yaml"),
        dir.join("terminus").join("config.yaml"),
    ]
}

/// 解析 config.yaml 的内容。
pub(super) fn parse(text: &str, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    // 合并键（<<）、锚点与别名在解析时展开
    let root = if text.trim().is_empty() {
        Value::Null
    } else {
        crate::yaml::to_json(text).map_err(|e| ImportError::Syntax(e.to_string()))?
    };
    let cfg = Config::from_value(&root);
    if cfg.encrypted && cfg.profiles.is_empty() {
        return Err(ImportError::TabbyEncrypted);
    }
    Ok(plan(&cfg, env))
}

struct Config {
    version: i64,
    encrypted: bool,
    profiles: Vec<Profile>,
    groups: Vec<Group>,
    /// 全局 SSH 默认值（profileDefaults.ssh）
    ssh_defaults: Option<Options>,
}

struct Profile {
    id: String,
    kind: String,
    name: String,
    group: String,
    options: Options,
}

struct Group {
    id: String,
    name: String,
    parent: String,
    /// 分组的 SSH 默认值（defaults.ssh）
    ssh_defaults: Option<Options>,
}

/// 区分"未设置"与空值，以便按 Tabby 的规则逐级补全默认值。
#[derive(Default)]
struct Options {
    host: Option<String>,
    port: Option<i64>,
    user: Option<String>,
    private_keys: Option<Vec<String>>,
    auth: Option<String>,
    jump_host: Option<String>,
    proxy_command: Option<String>,
    socks_proxy_host: Option<String>,
    socks_proxy_port: Option<i64>,
    http_proxy_host: Option<String>,
    http_proxy_port: Option<i64>,
}

impl Config {
    fn from_value(root: &Value) -> Self {
        let list = |key: &str| {
            field(root, key)
                .and_then(Value::as_array)
                .map(|s| s.iter().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        Self {
            version: field(root, "version").and_then(Value::as_i64).unwrap_or(0),
            encrypted: field(root, "encrypted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            profiles: list("profiles")
                .into_iter()
                .map(|p| Profile {
                    id: string(field(p, "id")),
                    kind: string(field(p, "type")),
                    name: string(field(p, "name")),
                    group: string(field(p, "group")),
                    options: Options::from_value(field(p, "options")),
                })
                .collect(),
            groups: list("groups")
                .into_iter()
                .map(|g| Group {
                    id: string(field(g, "id")),
                    name: string(field(g, "name")),
                    parent: string(field(g, "parentGroupId")),
                    ssh_defaults: ssh_defaults(field(g, "defaults")),
                })
                .collect(),
            ssh_defaults: ssh_defaults(field(root, "profileDefaults")),
        }
    }
}

/// `{ssh: {options: {...}}}` 中的 SSH 选项。
fn ssh_defaults(defaults: Option<&Value>) -> Option<Options> {
    let ssh = field(defaults?, "ssh")?;
    Some(Options::from_value(field(ssh, "options")))
}

impl Options {
    fn from_value(v: Option<&Value>) -> Self {
        let Some(v) = v else {
            return Self::default();
        };
        let s = |key: &str| scalar_string(field(v, key));
        let n = |key: &str| flex_int(field(v, key));
        Self {
            host: s("host"),
            port: n("port"),
            user: s("user"),
            private_keys: field(v, "privateKeys")
                .and_then(Value::as_array)
                .map(|keys| keys.iter().filter_map(|k| scalar_string(Some(k))).collect()),
            auth: s("auth"),
            jump_host: s("jumpHost"),
            proxy_command: s("proxyCommand"),
            socks_proxy_host: s("socksProxyHost"),
            socks_proxy_port: n("socksProxyPort"),
            http_proxy_host: s("httpProxyHost"),
            http_proxy_port: n("httpProxyPort"),
        }
    }
}

fn field<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    v.get(key)
}

/// 标量的文本（数字、布尔值也按文本）；没有设置、为 null 或不是标量时为 `None`。
fn scalar_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn string(v: Option<&Value>) -> String {
    scalar_string(v).unwrap_or_default()
}

/// 数字，兼容写成字符串的数字；无法解析时视为未设置。
fn flex_int(v: Option<&Value>) -> Option<i64> {
    match v? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 补全默认值后的 SSH 连接信息。
struct Resolved {
    host: String,
    port: u16,
    user: String,
    keys: Vec<String>,
    /// 认证方式，为空表示自动
    auth: String,
    jump: String,
    proxy_command: String,
    /// SOCKS / HTTP 代理及导入时的提醒
    proxy: Option<(Proxy, Option<Hint>)>,
}

/// 按优先级从高到低（`layers[0]` 最高）逐项取第一个设置了的值，再补上内置默认值。
fn merge(layers: &[&Options]) -> Resolved {
    let mut o = Resolved {
        host: String::new(),
        port: 22,
        user: "root".into(),
        keys: Vec::new(),
        auth: String::new(),
        jump: String::new(),
        proxy_command: String::new(),
        proxy: None,
    };
    let mut port = 0;
    let (mut socks_host, mut http_host) = (String::new(), String::new());
    let (mut socks_port, mut http_port) = (0, 0);
    let set = |dst: &mut String, src: &Option<String>| {
        if let Some(v) = src {
            *dst = v.trim().to_string();
        }
    };
    for l in layers.iter().rev() {
        set(&mut o.host, &l.host);
        set(&mut o.user, &l.user);
        set(&mut o.jump, &l.jump_host);
        set(&mut o.auth, &l.auth);
        set(&mut o.proxy_command, &l.proxy_command);
        set(&mut socks_host, &l.socks_proxy_host);
        set(&mut http_host, &l.http_proxy_host);
        if let Some(p) = l.port {
            port = p;
        }
        if let Some(keys) = &l.private_keys {
            o.keys = keys
                .iter()
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
                .collect();
        }
        if let Some(p) = l.socks_proxy_port {
            socks_port = p;
        }
        if let Some(p) = l.http_proxy_port {
            http_port = p;
        }
    }
    o.port = valid_port(port).unwrap_or(22);
    // 与 Tabby 相同的优先级：代理命令、SOCKS 代理、HTTP 代理
    if o.proxy_command.is_empty() {
        if !socks_host.is_empty() {
            o.proxy = Some(imported_proxy(
                ProxyKind::Socks5,
                socks_host,
                socks_port,
                1080,
            ));
        } else if !http_host.is_empty() {
            o.proxy = Some(imported_proxy(ProxyKind::Http, http_host, http_port, 8080));
        }
    }
    o
}

fn valid_port(port: i64) -> Option<u16> {
    u16::try_from(port).ok().filter(|&p| p > 0)
}

/// 端口没有设置或无效时用默认端口，并给出提醒。
fn imported_proxy(
    kind: ProxyKind,
    address: String,
    port: i64,
    default_port: u16,
) -> (Proxy, Option<Hint>) {
    let (port, hint) = match valid_port(port) {
        Some(p) => (p, None),
        None => (
            default_port,
            Some(Hint::ProxyPortDefaulted { port: default_port }),
        ),
    };
    let proxy = Proxy {
        kind,
        address,
        port,
        ..Default::default()
    };
    (proxy, hint)
}

fn plan(cfg: &Config, env: &LocalEnv) -> ImportPlan {
    let mut groups: HashMap<&str, &Group> = HashMap::new();
    for g in &cfg.groups {
        if !g.id.is_empty() {
            groups.insert(&g.id, g);
        }
    }
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    let mut by_name: HashMap<&str, usize> = HashMap::new();
    for (i, p) in cfg.profiles.iter().enumerate() {
        if p.kind != "ssh" {
            continue;
        }
        if !p.id.is_empty() {
            by_id.entry(&p.id).or_insert(i);
        }
        by_name.entry(&p.name).or_insert(i);
    }
    let resolve = |p: &Profile| {
        let mut layers = vec![&p.options];
        if let Some(d) = groups
            .get(p.group.as_str())
            .and_then(|g| g.ssh_defaults.as_ref())
        {
            layers.push(d);
        }
        if let Some(d) = &cfg.ssh_defaults {
            layers.push(d);
        }
        merge(&layers)
    };

    let mut plan = PlanBuilder::new(Source::Tabby);
    for (i, p) in cfg.profiles.iter().enumerate() {
        match p.kind.as_str() {
            "ssh" => {}
            // 本地终端与布局不是远程会话
            "local" | "split-layout" | "" => continue,
            // telnet、serial 等
            _ => {
                plan.skip(1);
                continue;
            }
        }
        let o = resolve(p);
        if o.host.is_empty() {
            plan.skip(1);
            continue;
        }
        let key = if p.id.is_empty() {
            format!("profile-{}", i + 1)
        } else {
            p.id.clone()
        };
        let name = p.name.trim();
        let host = Host {
            name: if name.is_empty() {
                o.host.clone()
            } else {
                name.to_string()
            },
            address: o.host.clone(),
            port: o.port,
            username: o.user.clone(),
            ..Default::default()
        };
        let mut item = PlannedHost::new(key, host);
        item.group = plan.group(&group_path(&groups, cfg.version, &p.group));

        if o.auth == "agent" {
            // Tabby 此时只用 agent 中的密钥（列出的私钥只用来挑选 agent 中对应的那一把）
            item.host.auth = Auth::Agent;
        } else if let Some(first) = o.keys.first() {
            match key_path(first, &o.host, &o.user, env) {
                Some(path) => {
                    item.host.auth = Auth::Key {
                        source: KeySource::file(path),
                    }
                }
                None => item.hints.push(Hint::PrivateKeyNotImported { name: None }),
            }
            if o.keys.len() > 1 {
                item.hints.push(Hint::ExtraPrivateKeys {
                    total: o.keys.len(),
                });
            }
        } else if o.auth == "publicKey" {
            item.hints.push(Hint::PrivateKeyFileMissing);
        }

        if !o.jump.is_empty() {
            // 旧版本按名称引用跳板机
            let target = by_id
                .get(o.jump.as_str())
                .or_else(|| by_name.get(o.jump.as_str()))
                .map(|&j| (&cfg.profiles[j], resolve(&cfg.profiles[j])));
            match target {
                Some((jp, jo)) if !jo.host.is_empty() => {
                    let name = match jp.name.trim() {
                        "" => jo.host.clone(),
                        n => n.to_string(),
                    };
                    if jo.user.is_empty() {
                        item.hints.push(Hint::JumpMissingUsername { name });
                    } else {
                        item.jump = Some(JumpRef::new(
                            name,
                            Endpoint::new(&jo.host, jo.port, &jo.user),
                        ));
                    }
                }
                _ => item.hints.push(Hint::JumpNotFound),
            }
        }

        if !o.proxy_command.is_empty() {
            item.host.proxy = Some(Proxy {
                kind: ProxyKind::Command,
                command: o.proxy_command.clone(),
                ..Default::default()
            });
            item.hints.push(Hint::ProxyCommandImported);
        } else if let Some((proxy, hint)) = o.proxy {
            item.host.proxy = Some(proxy);
            item.hints.extend(hint);
        }
        if item.host.username.is_empty() {
            item.hints.push(Hint::MissingUsername);
        }
        plan.push(item);
    }
    plan.finish()
}

/// 分组 id 解析为各级名称（含上级分组）。配置版本 5 起 group 是分组 id，找不到（分组已删除）时不分组；
/// 更早的版本中 group 直接是分组名。
fn group_path(groups: &HashMap<&str, &Group>, version: i64, reference: &str) -> Vec<String> {
    let reference = reference.trim();
    let Some(mut g) = groups.get(reference).copied() else {
        return if version >= 5 || reference.is_empty() {
            Vec::new()
        } else {
            vec![reference.to_string()]
        };
    };
    let mut names = Vec::new();
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(g.id.as_str()) {
            break;
        }
        let name = g.name.trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
        match groups.get(g.parent.as_str()) {
            Some(parent) => g = parent,
            None => break,
        }
    }
    names.reverse();
    names
}

/// Tabby 保存的私钥引用转为本机路径：本机文件保存为 `file://` 加原始路径；`%h`、`%r` 替换为主机地址与用户名
/// （与 Tabby 相同）。其他形式（例如 Tabby Vault 中的私钥）无法沿用，返回 `None`。
fn key_path(reference: &str, host: &str, user: &str, env: &LocalEnv) -> Option<String> {
    let r = reference.replace("%h", host).replace("%r", user);
    if let Some(rest) = r.strip_prefix("file://") {
        // file:///C:/… 形式
        let b = rest.as_bytes();
        let rest = if b.len() >= 3 && b[0] == b'/' && b[2] == b':' {
            &rest[1..]
        } else {
            rest
        };
        return Some(env.expand_home(rest));
    }
    if r.contains("://") {
        return None;
    }
    Some(env.expand_home(&r))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::{env, write};

    fn testdata(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/import/testdata/tabby")
            .join(name)
    }

    fn key_path_of(h: &PlannedHost) -> Option<&str> {
        match &h.host.auth {
            Auth::Key {
                source: KeySource::File { path, .. },
            } => Some(path),
            _ => None,
        }
    }

    fn group_of(plan: &ImportPlan, h: &PlannedHost) -> String {
        h.group
            .map_or_else(String::new, |g| plan.group_path(g).join("/"))
    }

    #[test]
    fn config_yaml() {
        let plan = read(&testdata(""), &env()).unwrap();
        assert_eq!(
            plan.skipped, 3,
            "应跳过串口、Telnet 与缺少地址的配置共 3 个（本地终端与布局不计）"
        );
        let got: Vec<_> = plan
            .hosts
            .iter()
            .map(|h| {
                (
                    h.key.as_str(),
                    h.host.name.as_str(),
                    h.host.address.as_str(),
                    h.host.port,
                    h.host.username.as_str(),
                    group_of(&plan, h),
                    key_path_of(h),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    "ssh:custom:bastion:11111111-1111-4111-8111-111111111111",
                    "堡垒机",
                    "bastion.example.com",
                    2222,
                    "jump",
                    "生产".to_string(),
                    Some(r"C:\Users\me\.ssh\bastion")
                ),
                // 端口与用户名来自全局默认值（所在分组 Web 没有默认值，上级分组的默认值不继承）
                (
                    "ssh:custom:order:22222222-2222-4222-8222-222222222222",
                    "订单服务",
                    "10.0.1.10",
                    2200,
                    "admin",
                    "生产/Web".to_string(),
                    Some("C:/Users/me/.ssh/id_ed25519")
                ),
                // 用户名来自所在分组的默认值
                (
                    "ssh:custom:db:33333333-3333-4333-8333-333333333333",
                    "数据库",
                    "db.internal",
                    22,
                    "deploy",
                    "生产".to_string(),
                    None
                ),
                // 引用的分组已删除时不分组
                (
                    "ssh:custom:broken:44444444-4444-4444-8444-444444444444",
                    "失效跳板",
                    "10.0.3.3",
                    2200,
                    "u",
                    String::new(),
                    None
                ),
            ]
        );
        assert_eq!(
            plan.hosts[1].jump,
            Some(JumpRef {
                name: "堡垒机".into(),
                endpoint: Endpoint::new("bastion.example.com", 2222, "jump"),
                host: Some(0),
            })
        );
        let db = &plan.hosts[2];
        let proxy = db.host.proxy.as_ref().unwrap();
        assert_eq!(
            (proxy.kind, proxy.address.as_str(), proxy.port),
            (ProxyKind::Socks5, "127.0.0.1", 1080)
        );
        let hints: Vec<_> = plan.hosts.iter().map(|h| h.hints.clone()).collect();
        assert_eq!(
            hints,
            [
                vec![],
                vec![Hint::ExtraPrivateKeys { total: 2 }],
                vec![Hint::PrivateKeyNotImported { name: None }],
                vec![Hint::JumpNotFound],
            ]
        );
        assert!(
            plan.hosts
                .iter()
                .all(|h| h.jump.is_none() || h.key.contains("order"))
        );
    }

    /// Tabby 1.0.238 的界面实际创建的配置：上级分组的路径、只取所在分组的默认值、按 id 引用的跳板机、
    /// agent 认证、两种代理；只有代理命令没有地址的配置跳过。
    #[test]
    fn real_1_0_238() {
        let plan = read(&testdata("real-1.0.238.yaml"), &env()).unwrap();
        assert_eq!(plan.skipped, 1, "只有代理命令、没有地址的 via-cmd 应跳过");
        assert_eq!(plan.hosts.len(), 5);
        let get = |name: &str| plan.hosts.iter().find(|h| h.host.name == name).unwrap();

        let web = get("web-01");
        assert_eq!(group_of(&plan, web), "生产/华东");
        assert_eq!(
            (web.host.port, web.host.username.as_str()),
            (2201, "deploy")
        );
        assert_eq!(web.host.auth, Auth::Password);

        // 用户名与端口来自所在分组"生产"的 SSH 默认值；agent 认证；跳板机按 id 引用
        let db = get("db-01");
        assert_eq!(group_of(&plan, db), "生产");
        assert_eq!((db.host.port, db.host.username.as_str()), (2022, "ops"));
        assert_eq!(db.host.auth, Auth::Agent);
        let jump = db.jump.as_ref().unwrap();
        assert_eq!(jump.name, "web-01");
        assert_eq!(jump.endpoint, Endpoint::new("10.1.2.3", 2201, "deploy"));
        assert_eq!(
            jump.host.map(|i| plan.hosts[i].host.name.as_str()),
            Some("web-01")
        );

        // 所在分组"测试"没有默认值：内置默认用户 root
        let http = get("via-http");
        assert_eq!(
            (http.host.username.as_str(), http.host.port),
            ("root", 2222)
        );
        let proxy = http.host.proxy.as_ref().unwrap();
        assert_eq!(
            (proxy.kind, proxy.address.as_str(), proxy.port),
            (ProxyKind::Http, "10.0.0.1", 3128)
        );

        let socks = get("via-socks");
        assert_eq!(
            (group_of(&plan, socks), socks.host.username.as_str()),
            (String::new(), "ops2")
        );
        let proxy = socks.host.proxy.as_ref().unwrap();
        assert_eq!(
            (proxy.kind, proxy.address.as_str(), proxy.port),
            (ProxyKind::Socks5, "127.0.0.1", 1080)
        );

        // 设为私钥认证但没有私钥文件：提醒导入后选择私钥
        let key = get("key-auth");
        assert_eq!(key.host.auth, Auth::Password);
        assert_eq!(key.hints, [Hint::PrivateKeyFileMissing]);
    }

    #[test]
    fn legacy() {
        let plan = read(&testdata("legacy.yaml"), &env()).unwrap();
        assert_eq!(plan.hosts.len(), 2);
        let (jump, app) = (&plan.hosts[0], &plan.hosts[1]);
        // 没有任何地方指定用户名与端口时与 Tabby 相同，使用 root 与 22
        assert_eq!(jump.key, "profile-1");
        assert_eq!((jump.host.username.as_str(), jump.host.port), ("root", 22));
        assert_eq!(group_of(&plan, jump), "旧分组");
        assert_eq!(
            app.jump,
            Some(JumpRef {
                name: "legacy-jump".into(),
                endpoint: Endpoint::new("10.1.0.1", 22, "root"),
                host: Some(0),
            }),
            "按名称引用的跳板机"
        );
        let path = key_path_of(app).unwrap();
        assert!(
            path.ends_with("10.1.0.2_app") && !path.starts_with('~'),
            "私钥路径应替换 %h、%r 并展开 ~：{path}"
        );
    }

    #[test]
    fn errors() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted = dir.path().join("encrypted.yaml");
        write(
            &encrypted,
            b"encrypted: true\nversion: 7\nvault:\n  contents: AAAA\n  keySalt: BBBB\n",
        );
        assert!(matches!(
            read(&encrypted, &env()),
            Err(ImportError::TabbyEncrypted)
        ));
        let broken = dir.path().join("broken.yaml");
        write(&broken, b"profiles: [\n  - name: x\n");
        assert!(matches!(read(&broken, &env()), Err(ImportError::Syntax(_))));
        // 空文件与类型不符的字段不报错
        assert!(parse("", &env()).unwrap().hosts.is_empty());
        let plan = parse(
            "profiles:\n  - type: ssh\n    name: [x]\n    options: {host: h, port: abc, user: 7, privateKeys: k}\n",
            &env(),
        )
        .unwrap();
        let h = &plan.hosts[0];
        assert_eq!(
            (h.host.name.as_str(), h.host.port, h.host.username.as_str()),
            ("h", 22, "7")
        );
        assert_eq!(h.host.auth, Auth::Password);
    }

    #[test]
    fn proxy_command_and_missing_user() {
        let plan = parse(
            "profiles:\n  - type: ssh\n    name: a\n    options: {host: h, user: '', proxyCommand: 'nc %h %p', socksProxyHost: s}\n\
             \x20 - type: ssh\n    name: b\n    options: {host: h2, httpProxyHost: p}\n",
            &env(),
        )
        .unwrap();
        let a = &plan.hosts[0];
        assert_eq!(
            a.host.proxy.as_ref().map(|p| (p.kind, p.command.as_str())),
            Some((ProxyKind::Command, "nc %h %p"))
        );
        assert_eq!(a.hints, [Hint::ProxyCommandImported, Hint::MissingUsername]);
        let b = &plan.hosts[1];
        assert_eq!(
            b.host.proxy.as_ref().map(|p| (p.kind, p.port)),
            Some((ProxyKind::Http, 8080))
        );
        assert_eq!(b.hints, [Hint::ProxyPortDefaulted { port: 8080 }]);
    }

    /// YAML 的边界情况。“旧：”标出与原来的 serde_yaml_ng 不同的结果（Tabby 用 js-yaml 写配置，
    /// 会给像数字的文本加引号，不会写出这些写法）。
    #[test]
    fn yaml_edge_cases() {
        let one = |options: &str| {
            format!("profiles:\n  - type: ssh\n    name: n\n    options: {options}\n")
        };
        let host = |host: &str| format!("{{type: ssh, name: a, options: {{host: {host}}}}}");
        let cases = [
            // 重复的键报错
            (format!("profiles:\n  - {}\nprofiles: []\n", host("h1")), "语法错误"),
            (one("{host: h1, host: h2}"), "语法错误"),
            // 锚点、别名与合并键
            (
                "x: &d {host: h1, user: admin}\nprofiles:\n  - {type: ssh, name: a, options: *d}\n"
                    .into(),
                "a h1:22 admin",
            ),
            (
                "b: &b {host: h1, user: admin, port: 2200}\nprofiles:\n  - type: ssh\n    name: a\n    options:\n      <<: *b\n      user: root2\n"
                    .into(),
                "a h1:2200 root2",
            ),
            (
                "a: &a {host: h1}\nb: &b {user: u2, host: h2}\nprofiles:\n  - type: ssh\n    name: a\n    options:\n      <<: [*a, *b]\n"
                    .into(),
                "a h1:22 u2",
            ),
            ("profiles:\n  - {type: ssh, name: a, options: *nope}\n".into(), "语法错误"),
            // 多个文档报错；只有一个文档时可以有 --- 与 ...
            (format!("profiles:\n  - {}\n---\nprofiles: []\n", host("h1")), "语法错误"),
            (format!("---\nprofiles:\n  - {}\n...\n", host("h1")), "a h1:22 root"),
            // 数字的写法
            (one("{host: h, port: 0x8AE}"), "n h:2222 root"),
            (one("{host: h, port: 0o4256}"), "n h:2222 root"),
            (one("{host: h, port: '2222'}"), "n h:2222 root"),
            (one("{host: h, port: +2222}"), "n h:2222 root"),
            (one("{host: h, port: 2222.0}"), "n h:22 root"),
            (one("{host: h, port: 2_222}"), "n h:2222 root"), // 旧：22
            (one("{host: h, port: 02222}"), "n h:22 root"),   // 旧：2222
            (one("{host: h, user: 1e3}"), "n h:22 1000.0"),
            (one("{host: h, user: 0x1F}"), "n h:22 31"),
            (one("{host: h, user: -5}"), "n h:22 -5"),
            (one("{host: h, user: 1.50}"), "n h:22 1.5"),
            (one("{host: h, user: .inf}"), "n h:22 .inf"),
            (one("{host: h, user: .nan}"), "n h:22 .nan"),
            (one("{host: h, user: 12345678901234567890}"), "n h:22 12345678901234567890"),
            (one("{host: h, user: 012}"), "n h:22 12.0"), // 旧：012
            (
                one("{host: h, user: 123456789012345678901234567890}"),
                "n h:22 1.2345678901234568e+29", // 旧：语法错误
            ),
            // 布尔值：只有 true / false（大小写变体）是布尔值，yes、on 等是文本
            (one("{host: h, user: yes}"), "n h:22 yes"),
            (one("{host: h, user: on}"), "n h:22 on"),
            (one("{host: h, user: y}"), "n h:22 y"),
            (one("{host: h, user: True}"), "n h:22 true"),
            ("encrypted: yes\nversion: 7\n".into(), "无"),
            ("encrypted: True\nversion: 7\n".into(), "已加密"),
            // 空值
            (one("{host: h, user: ~}"), "n h:22 root"),
            (one("{host: h, user: null}"), "n h:22 root"),
            (
                "profiles:\n  - type: ssh\n    name: n\n    options:\n      host: h\n      user:\n".into(),
                "n h:22 root",
            ),
            (one("{host: h, user: ''}"), "n h:22 "),
            (one("{host: ~}"), "跳过 1"),
            ("profiles:\n  - type: ssh\n    name: n\n    options:\n".into(), "跳过 1"),
            // 标签：取其中的值
            (
                "profiles:\n  - type: ssh\n    name: !!str 123\n    options: {host: h}\n".into(),
                "123 h:22 root",
            ),
            (
                "profiles:\n  - type: ssh\n    name: !custom web\n    options: !opts {host: h}\n".into(),
                "web h:22 root",
            ),
            (
                "profiles: !!seq\n  - !!map {type: ssh, name: n, options: {host: h}}\n".into(),
                "n h:22 root",
            ),
            // 不是字符串的键：数字可以；null 与集合作键时整个文件报错（旧：忽略这些键）
            (format!("1: x\nprofiles:\n  - {}\n", host("h1")), "a h1:22 root"),
            (format!("~: x\nprofiles:\n  - {}\n", host("h1")), "语法错误"),
            (format!("? [a, b]\n: c\nprofiles:\n  - {}\n", host("h1")), "语法错误"),
            // 其他
            ("- a\n- b\n".into(), "无"),
            ("hello\n".into(), "无"),
            ("# 只有注释\n".into(), "无"),
            (format!("\u{feff}profiles:\n  - {}\n", host("h1")), "a h1:22 root"),
            (
                "profiles:\r\n  - type: ssh\r\n    name: a\r\n    options: {host: h1}\r\n".into(),
                "a h1:22 root",
            ),
            (format!("profiles:\n\t- {}\n", host("h1")), "语法错误"),
        ];
        for (text, expected) in cases {
            let got = match parse(&text, &env()) {
                Ok(plan) if plan.hosts.is_empty() && plan.skipped > 0 => {
                    format!("跳过 {}", plan.skipped)
                }
                Ok(plan) if plan.hosts.is_empty() => "无".to_string(),
                Ok(plan) => plan
                    .hosts
                    .iter()
                    .map(|h| {
                        let host = &h.host;
                        format!(
                            "{} {}:{} {}",
                            host.name, host.address, host.port, host.username
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
                Err(ImportError::Syntax(_)) => "语法错误".to_string(),
                Err(ImportError::TabbyEncrypted) => "已加密".to_string(),
                Err(e) => format!("{e:?}"),
            };
            assert_eq!(got, expected, "{text:?}");
        }

        // 语法错误的说明只有位置，不带配置中的内容
        let Err(ImportError::Syntax(message)) = parse(
            "password: hunter2-secret\nprofiles: [\n  - name: x\n",
            &env(),
        ) else {
            panic!("应为语法错误");
        };
        assert!(!message.contains("hunter2"), "{message}");
    }

    #[test]
    fn default_candidates_use_config_dir() {
        let mut env = env();
        let dir = tempfile::tempdir().unwrap();
        env.config_dir = Some(dir.path().to_path_buf());
        let path = dir.path().join("tabby").join("config.yaml");
        write(&path, b"profiles: []\n");
        assert_eq!(
            super::super::first_existing(default_candidates(&env)),
            Some(path)
        );
    }
}
