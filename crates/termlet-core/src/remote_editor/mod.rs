//! 在 VS Code、Zed 中打开远程目录（文件管理的菜单）。编辑器用它自己的 ssh 连接服务器，Termlet 只拼出编辑器认得的
//! 链接，交给系统打开：
//!
//! - VS Code（Remote - SSH 扩展）：`vscode://vscode-remote/ssh-remote+<主机><路径>`。主机含大写字母或
//!   `a-z0-9._-@` 以外的字符时写成 VS Code 自己也用的十六进制 JSON（`{"hostName":"…"}`）：链接的这一段会被当作
//!   不区分大小写。WSL 发行版是 `vscode://vscode-remote/wsl+<发行版><路径>`。
//! - Zed：`zed://ssh/[<用户>@]<主机>[:<端口>]<路径>`（Zed 文档的 Remote Development）；Zed 没有打开 WSL 的链接。
//!
//! 主机的写法（[`plan`]）：优先用 `~/.ssh/config` 中与这台主机（地址、用户、端口）对应的 Host 别名
//! （[`SshConfig::find_alias`]），编辑器连接时按其中的设置；没有时写 `用户@地址`（Zed 带上非 22 的端口；VS Code 的
//! 文档只有 `用户@地址`，非 22 端口按 Remote - SSH “连接到主机”的写法 `用户@地址:端口`，并建议写入配置）。
//! 没有别名时列出编辑器可能连不上的原因（[`Obstacle`]），并给出可以追加到 `~/.ssh/config` 的条目（[`Entry`]，
//! 见 [`ssh_config::append`]）：只含地址、用户、端口、本机私钥文件的路径与跳板机（`ProxyJump`），不含密码，也不含
//! 保存在 Termlet 中的私钥与代理命令（命令中可能带令牌）。

pub mod detect;
pub mod ssh_config;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::model::{ProxyKind, RemoteEditorSetting};
use crate::ssh::plan::{AuthPlan, ConnectPlan, KeyMaterial};

pub use ssh_config::{Entry, SshConfig};

/// 能打开远程目录的编辑器。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Editor {
    VsCode,
    Zed,
}

impl Editor {
    pub const ALL: [Editor; 2] = [Editor::VsCode, Editor::Zed];

    /// 产品名（菜单中的“在 VS Code 中打开”）。
    pub fn name(self) -> &'static str {
        match self {
            Editor::VsCode => "VS Code",
            Editor::Zed => "Zed",
        }
    }

    /// 编辑器注册的 URL 协议。
    pub fn scheme(self) -> &'static str {
        match self {
            Editor::VsCode => "vscode",
            Editor::Zed => "zed",
        }
    }

    /// 能不能打开 WSL 发行版中的目录。
    pub fn supports_wsl(self) -> bool {
        self == Editor::VsCode
    }

    /// 设置允许显示它。
    pub fn allowed_by(self, setting: RemoteEditorSetting) -> bool {
        match setting {
            RemoteEditorSetting::Auto => true,
            RemoteEditorSetting::VsCode => self == Editor::VsCode,
            RemoteEditorSetting::Zed => self == Editor::Zed,
            RemoteEditorSetting::Off => false,
        }
    }
}

/// 编辑器要连的地方。
#[derive(Clone, Debug, PartialEq)]
pub enum Remote {
    Ssh(Endpoint),
    Wsl { distro: String },
}

/// 一台 SSH 主机：从 Termlet 的连接计划中取出编辑器的 ssh 需要知道的部分。
#[derive(Clone, Debug, PartialEq)]
pub struct Endpoint {
    /// Termlet 中的名称（建议的 Host 别名由它而来）
    pub name: String,
    pub address: String,
    pub port: u16,
    pub user: String,
    /// 登录用的本机私钥文件
    pub identity_file: Option<PathBuf>,
    /// 另外指定的证书文件（私钥文件旁同名的 `-cert.pub` 由 ssh 自己找，不写）
    pub certificate_file: Option<PathBuf>,
    /// 经过的跳板机（先经过的在前）
    pub jumps: Vec<JumpHop>,
    /// 第一跳的代理
    pub proxy: Option<ProxyKind>,
    /// 目标或跳板机用了保存在 Termlet 中的私钥（密钥库中的、粘贴的）
    pub stored_key: bool,
}

/// 一台跳板机。
#[derive(Clone, Debug, PartialEq)]
pub struct JumpHop {
    pub label: String,
    pub address: String,
    pub port: u16,
    pub user: String,
}

impl Endpoint {
    /// 从连接计划中取出（`name` 为 Termlet 中的名称）。只读计划中的地址、用户、端口与认证方式，不碰其中的秘密。
    pub fn from_plan(name: &str, plan: &ConnectPlan) -> Self {
        let target = plan.target();
        let (identity_file, certificate_file) = match &target.auth {
            AuthPlan::Key {
                key: KeyMaterial::File(path),
                certificate,
                ..
            } => (
                Some(path.clone()),
                match certificate {
                    Some(crate::ssh::CertMaterial::File(file)) => Some(file.clone()),
                    _ => None,
                },
            ),
            _ => (None, None),
        };
        let stored_key = plan.hops.iter().any(|hop| {
            matches!(
                hop.auth,
                AuthPlan::Key {
                    key: KeyMaterial::Text(_),
                    ..
                }
            )
        });
        let jumps = plan.hops[..plan.hops.len() - 1]
            .iter()
            .map(|hop| JumpHop {
                label: hop.label.clone(),
                address: hop.address.clone(),
                port: hop.port,
                user: hop.username.clone(),
            })
            .collect();
        Self {
            name: name.to_string(),
            address: target.address.clone(),
            port: target.port,
            user: target.username.clone(),
            identity_file,
            certificate_file,
            jumps,
            // 代理只对第一跳生效（见 `ssh::plan`）
            proxy: plan
                .hops
                .first()
                .and_then(|hop| hop.proxy.as_ref())
                .map(|p| p.kind),
            stored_key,
        }
    }

    /// 地址的写法：IPv6 加方括号（带端口、`ProxyJump`、URL 中）。
    fn bracketed_address(&self) -> String {
        bracket(&self.address)
    }
}

/// 编辑器（不经 `~/.ssh/config` 时）可能连不上的原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Obstacle {
    /// 经 Termlet 中的跳板机连接（`hosts` 为跳板机的名称，先经过的在前）。写入的配置中用 `ProxyJump` 经过它们
    Jump { hosts: Vec<String> },
    /// 经 SOCKS、HTTP 代理连接：OpenSSH 没有对应的设置
    Proxy,
    /// 经代理命令连接：命令中可能带令牌，不写入配置
    ProxyCommand,
    /// 私钥保存在 Termlet 中，编辑器的 ssh 用不了
    StoredKey,
    /// 用的私钥文件不在 ssh 默认找的位置。写入的配置中带有 `IdentityFile`
    KeyFile { path: String },
    /// VS Code 与非 22 端口：VS Code 的文档只说明了在 `~/.ssh/config` 中设置端口。写入的配置中带有 `Port`
    Port { port: u16 },
}

impl Obstacle {
    /// 写入建议的配置条目后就不再是问题。
    pub fn fixed_by_entry(&self) -> bool {
        matches!(
            self,
            Obstacle::Jump { .. } | Obstacle::KeyFile { .. } | Obstacle::Port { .. }
        )
    }
}

/// 怎样在编辑器中打开。
#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub editor: Editor,
    /// 交给系统打开的链接
    pub url: String,
    /// 链接中主机的写法（显示用）：别名或 `用户@地址[:端口]`；WSL 为发行版的名称
    pub host: String,
    /// 用的是 `~/.ssh/config` 中的这个别名
    pub alias: Option<String>,
    /// 不经配置直接打开时可能连不上的原因（有别名、WSL 时为空）
    pub obstacles: Vec<Obstacle>,
    /// 没有别名时建议追加到 `~/.ssh/config` 的条目
    pub entry: Option<Entry>,
}

impl Launch {
    /// 要先问用户（显示原因与可以写入的配置）：SSH 主机在配置中没有别名时。
    pub fn needs_confirm(&self) -> bool {
        self.entry.is_some()
    }
}

/// 在 `editor` 中打开 `remote` 上的 `path`（绝对路径）。`config` 为 `~/.ssh/config` 的内容，`local_user` 为本机的
/// 用户名（配置中没写 User 时 ssh 用它），`stamp` 写在建议条目的注释中（时间）。编辑器打不开的（Zed 与 WSL）为 `None`。
pub fn plan(
    editor: Editor,
    remote: &Remote,
    path: &str,
    config: &SshConfig,
    local_user: &str,
    stamp: &str,
) -> Option<Launch> {
    let endpoint = match remote {
        Remote::Wsl { distro } => {
            if !editor.supports_wsl() {
                return None;
            }
            return Some(Launch {
                editor,
                url: format!(
                    "vscode://vscode-remote/wsl+{}{}",
                    encode_component(distro),
                    encode_path(path)
                ),
                host: distro.clone(),
                alias: None,
                obstacles: Vec::new(),
                entry: None,
            });
        }
        Remote::Ssh(endpoint) => endpoint,
    };
    let alias = config
        .find_alias(&endpoint.address, endpoint.port, &endpoint.user, local_user)
        .filter(|alias| editor == Editor::VsCode || zed_host_ok(alias));
    if let Some(alias) = alias {
        return Some(Launch {
            editor,
            url: alias_url(editor, &alias, path),
            host: alias.clone(),
            alias: Some(alias),
            obstacles: Vec::new(),
            entry: None,
        });
    }
    let (url, host) = match editor {
        Editor::VsCode => {
            let host = if endpoint.port == 22 {
                format!("{}@{}", endpoint.user, endpoint.address)
            } else {
                format!(
                    "{}@{}:{}",
                    endpoint.user,
                    endpoint.bracketed_address(),
                    endpoint.port
                )
            };
            (vscode_url(&host, path), host)
        }
        Editor::Zed => {
            let port = if endpoint.port == 22 {
                String::new()
            } else {
                format!(":{}", endpoint.port)
            };
            let url = format!(
                "zed://ssh/{}@{}{port}{}",
                encode_component(&endpoint.user),
                endpoint.bracketed_address(),
                encode_path(path)
            );
            let host = format!("{}@{}{port}", endpoint.user, endpoint.bracketed_address());
            (url, host)
        }
    };
    Some(Launch {
        editor,
        url,
        host,
        alias: None,
        obstacles: obstacles(editor, endpoint),
        entry: Some(Entry::for_endpoint(endpoint, config, stamp)),
    })
}

/// 用 `~/.ssh/config` 中的别名打开的链接（写入建议的条目之后也用它）。
pub fn alias_url(editor: Editor, alias: &str, path: &str) -> String {
    match editor {
        Editor::VsCode => vscode_url(alias, path),
        Editor::Zed => format!("zed://ssh/{alias}{}", encode_path(path)),
    }
}

/// 不经配置直接打开时可能连不上的原因。
fn obstacles(editor: Editor, endpoint: &Endpoint) -> Vec<Obstacle> {
    let mut found = Vec::new();
    if !endpoint.jumps.is_empty() {
        found.push(Obstacle::Jump {
            hosts: endpoint.jumps.iter().map(|j| j.label.clone()).collect(),
        });
    }
    match endpoint.proxy {
        Some(ProxyKind::Command) => found.push(Obstacle::ProxyCommand),
        Some(_) => found.push(Obstacle::Proxy),
        None => {}
    }
    if endpoint.stored_key {
        found.push(Obstacle::StoredKey);
    }
    if let Some(path) = &endpoint.identity_file
        && !is_default_identity(path)
    {
        found.push(Obstacle::KeyFile {
            path: path.to_string_lossy().into_owned(),
        });
    }
    if editor == Editor::VsCode && endpoint.port != 22 {
        found.push(Obstacle::Port {
            port: endpoint.port,
        });
    }
    found
}

/// ssh 没有指定私钥时找的文件（`~/.ssh/id_rsa` 等）。
fn is_default_identity(path: &Path) -> bool {
    const DEFAULTS: [&str; 6] = [
        "id_rsa",
        "id_ecdsa",
        "id_ecdsa_sk",
        "id_ed25519",
        "id_ed25519_sk",
        "id_dsa",
    ];
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let in_ssh_dir = path
        .parent()
        .is_some_and(|dir| same_path(dir, &home.join(".ssh")));
    in_ssh_dir
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| DEFAULTS.contains(&n))
}

/// 两个本机路径是否相同（Windows 不区分大小写、`/` 与 `\` 相同）。
fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let text = p
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_string();
        if cfg!(windows) {
            text.replace('/', "\\").to_lowercase()
        } else {
            text
        }
    };
    norm(a) == norm(b)
}

/// VS Code Remote - SSH 的链接：主机只含小写字母、数字与 `._-@` 时原样写，否则写成十六进制 JSON。
fn vscode_url(host: &str, path: &str) -> String {
    let plain = host
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-@".contains(&b));
    let authority = if plain && !host.is_empty() {
        host.to_string()
    } else {
        let json = serde_json::json!({ "hostName": host }).to_string();
        json.bytes().map(|b| format!("{b:02x}")).collect()
    };
    format!(
        "vscode://vscode-remote/ssh-remote+{authority}{}",
        encode_path(path)
    )
}

/// Zed 的链接中能直接写的主机别名（URL 的主机部分不能转义，含别的字符时改用 `用户@地址`）。
fn zed_host_ok(alias: &str) -> bool {
    !alias.is_empty()
        && alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// IPv6 地址加方括号（已经有的不再加）。
fn bracket(address: &str) -> String {
    if address.contains(':') && !address.starts_with('[') {
        format!("[{address}]")
    } else {
        address.to_string()
    }
}

/// URL 中的路径：以 `/` 开头，`/` 与不需要转义的字符原样，其余按 UTF-8 字节转义（空格、中文、`#`、`?`、`%`、`:` 等）。
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() + 1);
    if !path.starts_with('/') {
        out.push('/');
    }
    for b in path.bytes() {
        if b == b'/' || is_unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// URL 中的一段（用户名、发行版名）：不需要转义的字符原样，其余转义。
fn encode_component(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if is_unreserved(b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"-._~".contains(&b)
}

/// 建议条目的注释中写的时间（现在的本地时间，`2026-11-09 18:00`）。
pub fn stamp_now() -> String {
    crate::time::format_local(crate::time::now_ms())
}

/// 本机的用户名（配置中没有写 User 时 ssh 用它）。
pub fn local_username() -> String {
    std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).unwrap_or_default()
}
