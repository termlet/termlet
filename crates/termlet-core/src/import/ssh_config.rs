//! OpenSSH 的 config 文件。
//!
//! 每个不含通配符的 Host 别名导入为一台主机；HostName、Port、User、IdentityFile、CertificateFile、ProxyJump、ProxyCommand、
//! ForwardX11、ForwardX11Trusted 按 ssh 的规则取值：按出现顺序，第一个匹配的块中的值生效（包括 `Host *` 等通配块提供的默认值）。
//! Include 不跟随，Match 块不参与匹配。
//! ProxyJump 导入为跳板机（多级时只导入最后一跳，并提醒）；ProxyCommand 导入为代理命令（占位符与 ssh 相同）；
//! 两者都有时只导入 ProxyJump。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::model::{Auth, Host, KeySource, Proxy, ProxyKind};

use super::files::{LocalEnv, decode_text, read_config_file};
use super::plan::{Endpoint, Hint, JumpRef, PlanBuilder, PlannedHost};
use super::{ImportError, ImportPlan, Source};

pub(super) fn read(path: &Path, env: &LocalEnv) -> Result<ImportPlan, ImportError> {
    let (raw, _) = read_config_file(path, "config", env)?;
    Ok(parse(&decode_text(&raw, env.ansi), env))
}

pub(super) fn default_candidates(env: &LocalEnv) -> Vec<PathBuf> {
    env.home
        .iter()
        .map(|h| h.join(".ssh").join("config"))
        .collect()
}

/// 解析 config 文件的内容。
pub(super) fn parse(text: &str, env: &LocalEnv) -> ImportPlan {
    let cfg = Config::parse(text);
    let mut plan = PlanBuilder::new(Source::SshConfig);
    let mut seen = HashSet::new();
    for block in &cfg.blocks {
        let Some(patterns) = &block.patterns else {
            continue;
        };
        for pattern in patterns {
            let alias = pattern.text.as_str();
            if pattern.negated || alias.is_empty() || alias.contains(['*', '?', '!']) {
                continue;
            }
            if seen.insert(alias.to_string()) {
                plan.push(host_item(&cfg, alias, env));
            }
        }
    }
    plan.finish()
}

fn host_item(cfg: &Config, alias: &str, env: &LocalEnv) -> PlannedHost {
    let get = |key: &str| cfg.get(alias, key).map(unquote).unwrap_or_default();
    let address = get("hostname");
    let mut host = Host {
        name: alias.to_string(),
        address: if address.is_empty() {
            alias.to_string()
        } else {
            address
        },
        port: parse_port(&get("port")).unwrap_or(22),
        username: get("user"),
        ..Default::default()
    };
    let identity = get("identityfile");
    if !identity.is_empty() {
        // CertificateFile：与私钥一起出示的证书；没有写时连接时自动找私钥旁同名的 -cert.pub
        let certificate = get("certificatefile");
        host.auth = Auth::Key {
            source: KeySource::File {
                path: env.expand_home(&identity),
                certificate: if certificate.is_empty() {
                    String::new()
                } else {
                    env.expand_home(&certificate)
                },
            },
        };
    }
    // ForwardX11 / ForwardX11Trusted（yes / no，不区分大小写）
    let yes = |key: &str| get(key).eq_ignore_ascii_case("yes");
    host.ssh.x11_forward = yes("forwardx11");
    host.ssh.x11_trusted = yes("forwardx11trusted");
    let mut item = PlannedHost::new(alias, host);
    if item.host.username.is_empty() {
        item.hints.push(Hint::MissingUsername);
    }

    let jump = get("proxyjump");
    let command = cfg.get(alias, "proxycommand").unwrap_or_default();
    let has_command = !command.is_empty() && !command.eq_ignore_ascii_case("none");
    if !jump.is_empty() && !jump.eq_ignore_ascii_case("none") {
        let hops: Vec<&str> = jump.split(',').map(str::trim).collect();
        item.jump = Some(jump_ref(cfg, hops[hops.len() - 1]));
        if hops.len() > 1 {
            item.hints.push(Hint::MultiHopJump {
                chain: hops.iter().map(|h| h.to_string()).collect(),
            });
        }
        if has_command {
            item.hints.push(Hint::ProxyCommandIgnored);
        }
    } else if has_command {
        item.host.proxy = Some(Proxy {
            kind: ProxyKind::Command,
            command: command.to_string(),
            ..Default::default()
        });
        item.hints.push(Hint::ProxyCommandImported);
    }
    item
}

/// ProxyJump 中的一跳（`[user@]host[:port]`）；host 是配置中的别名时按该别名的设置补全。
fn jump_ref(cfg: &Config, hop: &str) -> JumpRef {
    let (user, host_port) = match hop.split_once('@') {
        Some((user, rest)) => (user, rest),
        None => ("", hop),
    };
    let (host, port) = match split_host_port(host_port) {
        Some((host, port)) => (host, parse_port(port)),
        None => (
            host_port
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host_port),
            None,
        ),
    };
    let get = |key: &str| cfg.get(host, key).map(unquote).unwrap_or_default();
    let address = get("hostname");
    let address = if address.is_empty() {
        host.to_string()
    } else {
        address
    };
    let port = port.or_else(|| parse_port(&get("port"))).unwrap_or(22);
    let user = if user.is_empty() {
        get("user")
    } else {
        user.to_string()
    };
    JumpRef::new(host, Endpoint::new(&address, port, &user))
}

/// 与 Go 的 `net.SplitHostPort` 相同：`host:port` 或 `[host]:port`，host 中不能再有冒号。
pub(super) fn split_host_port(s: &str) -> Option<(&str, &str)> {
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':')?;
        (!port.contains(['[', ']'])).then_some((host, port))
    } else {
        let (host, port) = s.rsplit_once(':')?;
        (!host.contains([':', '[', ']']) && !port.contains(['[', ']'])).then_some((host, port))
    }
}

pub(super) fn parse_port(s: &str) -> Option<u16> {
    s.trim().parse::<u16>().ok().filter(|&p| p > 0)
}

/// 去掉值两端的双引号（OpenSSH 允许用引号包住含空格的路径）。
fn unquote(value: &str) -> String {
    let v = value.trim();
    v.strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(v)
        .to_string()
}

struct Config {
    blocks: Vec<Block>,
}

struct Block {
    /// Host 块的模式；Match 块为 `None`（不支持，不参与匹配）
    patterns: Option<Vec<Pattern>>,
    /// 关键字（小写）与值
    entries: Vec<(String, String)>,
}

struct Pattern {
    text: String,
    negated: bool,
}

impl Config {
    fn parse(text: &str) -> Self {
        // 第一个 Host 之前的设置对所有主机生效
        let mut blocks = vec![Block {
            patterns: Some(vec![Pattern {
                text: "*".into(),
                negated: false,
            }]),
            entries: Vec::new(),
        }];
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = split_key_value(line) else {
                continue;
            };
            match key.to_ascii_lowercase().as_str() {
                "host" => blocks.push(Block {
                    patterns: Some(
                        value
                            .split_whitespace()
                            .map(|p| {
                                let p = p.trim_matches('"');
                                match p.strip_prefix('!') {
                                    Some(rest) => Pattern {
                                        text: rest.to_string(),
                                        negated: true,
                                    },
                                    None => Pattern {
                                        text: p.to_string(),
                                        negated: false,
                                    },
                                }
                            })
                            .collect(),
                    ),
                    entries: Vec::new(),
                }),
                "match" => blocks.push(Block {
                    patterns: None,
                    entries: Vec::new(),
                }),
                "include" => {}
                key => {
                    if let Some(block) = blocks.last_mut() {
                        block.entries.push((key.to_string(), value.to_string()));
                    }
                }
            }
        }
        Self { blocks }
    }

    /// 别名 `alias` 的设置 `key`（小写）：按出现顺序，第一个匹配的块中的第一个值。
    fn get(&self, alias: &str, key: &str) -> Option<&str> {
        self.blocks
            .iter()
            .filter(|b| b.matches(alias))
            .find_map(|b| {
                b.entries
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            })
    }
}

impl Block {
    /// 有模式匹配且没有否定的模式匹配（否定的模式匹配时整个块不适用）。
    fn matches(&self, alias: &str) -> bool {
        let Some(patterns) = &self.patterns else {
            return false;
        };
        let mut found = false;
        for p in patterns {
            if glob(&p.text, alias) {
                if p.negated {
                    return false;
                }
                found = true;
            }
        }
        found
    }
}

/// 拆出关键字与值：`Key Value`、`Key=Value`、`Key = Value`；值中以 `#` 开头的词（不在引号中）及之后是注释。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key = &line[..end];
    if key.is_empty() {
        return None;
    }
    let mut rest = line[end..].trim_start();
    if let Some(r) = rest.strip_prefix('=') {
        rest = r.trim_start();
    }
    let mut quoted = false;
    let mut prev_space = true;
    let mut cut = rest.len();
    for (i, c) in rest.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted && prev_space => {
                cut = i;
                break;
            }
            _ => {}
        }
        prev_space = c.is_whitespace();
    }
    Some((key, rest[..cut].trim_end()))
}

/// 通配符匹配：`*` 任意多个字符，`?` 一个字符，其余按字面（区分大小写）。
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || (p[pi] != '*' && p[pi] == t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::files::tests::env;

    fn key_path(h: &PlannedHost) -> Option<&str> {
        match &h.host.auth {
            Auth::Key {
                source: KeySource::File { path, .. },
            } => Some(path),
            _ => None,
        }
    }

    #[test]
    fn hosts_and_defaults() {
        let plan = parse(
            r"
Host web prod-web
  HostName 10.0.0.10
  Port 2222
  IdentityFile /keys/web

Host db
  HostName db.internal

Host *.corp jump?
  User ignored

Host *
  User deploy
",
            &env(),
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
                    key_path(h),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("web", "web", "10.0.0.10", 2222, "deploy", Some("/keys/web")),
                (
                    "prod-web",
                    "prod-web",
                    "10.0.0.10",
                    2222,
                    "deploy",
                    Some("/keys/web")
                ),
                ("db", "db", "db.internal", 22, "deploy", None),
            ]
        );
        assert!(
            plan.hosts
                .iter()
                .all(|h| h.hints.is_empty() && h.jump.is_none())
        );
        assert!(plan.groups.is_empty());
        assert_eq!(plan.skipped, 0);
    }

    /// ProxyJump 导入为跳板机（别名按其设置补全，多级只取最后一跳）；ProxyCommand 导入为代理命令。
    #[test]
    fn proxy_jump_and_command() {
        let plan = parse(
            r"
Host bastion
  HostName 1.2.3.4
  Port 2200
  User jump

Host app
  HostName 10.0.0.20
  User deploy
  ProxyJump bastion

Host chain
  HostName 10.0.0.21
  User deploy
  ProxyJump ops@gw.example.com:2022,bastion

Host ssm
  HostName i-0123
  User ec2-user
  ProxyCommand aws ssm start-session --target %h --document-name AWS-StartSSHSession --parameters portNumber=%p
  ForwardX11 yes
  ForwardX11Trusted Yes

Host both
  HostName 10.0.0.22
  User deploy
  ProxyJump admin@[fe80::1]:22
  ProxyCommand nc %h %p
",
            &env(),
        );
        let by_key = |k: &str| plan.hosts.iter().find(|h| h.key == k).unwrap();
        let bastion = JumpRef {
            name: "bastion".into(),
            endpoint: Endpoint::new("1.2.3.4", 2200, "jump"),
            host: Some(0),
        };
        let app = by_key("app");
        assert_eq!(
            app.jump.as_ref(),
            Some(&bastion),
            "别名跳板机应按其设置补全"
        );
        assert!(app.hints.is_empty());

        let chain = by_key("chain");
        assert_eq!(chain.jump.as_ref(), Some(&bastion));
        assert_eq!(
            chain.hints,
            [Hint::MultiHopJump {
                chain: vec!["ops@gw.example.com:2022".into(), "bastion".into()]
            }]
        );

        let ssm = by_key("ssm");
        let proxy = ssm.host.proxy.as_ref().unwrap();
        assert_eq!(proxy.kind, ProxyKind::Command);
        assert!(
            proxy
                .command
                .starts_with("aws ssm start-session --target %h")
        );
        assert!(proxy.command.ends_with("portNumber=%p"));
        assert_eq!(ssm.hints, [Hint::ProxyCommandImported]);
        assert!(ssm.host.ssh.x11_forward && ssm.host.ssh.x11_trusted);
        assert!(!app.host.ssh.x11_forward && !app.host.ssh.x11_trusted);

        let both = by_key("both");
        assert_eq!(
            both.jump.as_ref(),
            Some(&JumpRef {
                name: "fe80::1".into(),
                endpoint: Endpoint::new("fe80::1", 22, "admin"),
                host: None,
            })
        );
        assert!(both.host.proxy.is_none());
        assert_eq!(both.hints, [Hint::ProxyCommandIgnored]);
    }

    #[test]
    fn syntax_variants() {
        let plan = parse(
            "# 注释\nUser top\nHost=a !a-off\n  HostName = \"10.0.0.1\" # 地址\n  IdentityFile \"~/my keys/id\"\n\
             Match host a\n  User from-match\nHost b\n  Port 70000\n  ProxyJump none\nInclude other.conf\n",
            &env(),
        );
        assert_eq!(plan.hosts.len(), 2);
        let a = &plan.hosts[0];
        assert_eq!(a.host.address, "10.0.0.1");
        assert_eq!(
            a.host.username, "top",
            "第一个 Host 之前的设置对所有主机生效"
        );
        let path = key_path(a).unwrap();
        assert!(path.ends_with("id") && path.contains("my keys") && !path.starts_with('~'));
        let b = &plan.hosts[1];
        assert_eq!((b.host.address.as_str(), b.host.port), ("b", 22));
        assert!(b.jump.is_none());
    }

    // CertificateFile 随私钥文件导入；没有写时留空（连接时自动找同名的 -cert.pub）
    #[test]
    fn certificate_file() {
        let plan = parse(
            "Host a\n  IdentityFile ~/.ssh/id_ed25519\n  CertificateFile ~/.ssh/id_ed25519-cert.pub\n\
             Host b\n  IdentityFile /k/b\n",
            &env(),
        );
        let certificate = |h: &PlannedHost| match &h.host.auth {
            Auth::Key {
                source: KeySource::File { certificate, .. },
            } => certificate.clone(),
            other => panic!("{other:?}"),
        };
        let a = certificate(&plan.hosts[0]);
        assert!(
            a.ends_with("id_ed25519-cert.pub") && !a.starts_with('~'),
            "{a}"
        );
        assert_eq!(certificate(&plan.hosts[1]), "");
    }

    #[test]
    fn missing_username_and_negated_patterns() {
        let plan = parse("Host a\nHost * !a\n  User u\n", &env());
        assert_eq!(plan.hosts[0].host.username, "");
        assert_eq!(plan.hosts[0].hints, [Hint::MissingUsername]);
    }

    #[test]
    fn glob_matching() {
        assert!(glob("*", "anything"));
        assert!(glob("*.corp", "a.corp"));
        assert!(!glob("*.corp", "a.corpx"));
        assert!(glob("jump?", "jump1"));
        assert!(!glob("jump?", "jump"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("Web", "web"));
    }

    #[test]
    fn host_port_splitting() {
        assert_eq!(split_host_port("h:22"), Some(("h", "22")));
        assert_eq!(split_host_port("[fe80::1]:22"), Some(("fe80::1", "22")));
        assert_eq!(split_host_port("fe80::1"), None);
        assert_eq!(split_host_port("h"), None);
        let cfg = Config::parse("");
        assert_eq!(
            jump_ref(&cfg, "u@[::1]").endpoint,
            Endpoint::new("::1", 22, "u")
        );
    }
}
