//! `~/.ssh/config`：找与一台主机对应的 Host 别名（[`SshConfig::find_alias`]），生成建议追加的条目（[`Entry`]）
//! 并追加（[`append`]）。
//!
//! - 只认第一个 `Host` 之前的全局设置与 `Host` 块：同一项按出现顺序，第一个匹配的块中的值生效（与 ssh 相同，包括
//!   `Host *` 等通配块提供的默认值）。`Match` 块不参与，`Include` 不跟随（被包含的文件中的别名找不到，最多是建议
//!   追加一条重复的）。别名按 ssh 的规则不区分大小写。
//! - 追加只在文件末尾写，写之前把原文件复制一份（`config.termlet-backup-<时间>`）；文件不存在时新建（Linux、macOS
//!   上目录为 0700、文件为 0600）。条目中不写密码与保存在 Termlet 中的私钥。

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use super::Endpoint;

/// 解析后的 ssh 配置。
#[derive(Clone, Debug, Default)]
pub struct SshConfig {
    blocks: Vec<Block>,
}

#[derive(Clone, Debug)]
enum Scope {
    /// 第一个 `Host` 之前：对所有主机生效
    Global,
    Host(Vec<Pattern>),
    /// `Match` 块：不参与
    Match,
}

#[derive(Clone, Debug)]
struct Pattern {
    text: String,
    negated: bool,
}

#[derive(Clone, Debug)]
struct Block {
    scope: Scope,
    /// 键为小写
    options: Vec<(String, String)>,
}

impl SshConfig {
    /// 读取文件；不存在时为空配置。
    pub fn read(path: &Path) -> io::Result<Self> {
        match fs::read(path) {
            Ok(raw) => Ok(Self::parse(&String::from_utf8_lossy(&raw))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    pub fn parse(text: &str) -> Self {
        let mut blocks = vec![Block {
            scope: Scope::Global,
            options: Vec::new(),
        }];
        for line in text.lines() {
            let line = line.trim().trim_start_matches('\u{feff}');
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = split_key_value(line) else {
                continue;
            };
            let key = key.to_ascii_lowercase();
            match key.as_str() {
                "host" => blocks.push(Block {
                    scope: Scope::Host(
                        split_words(value)
                            .into_iter()
                            .map(|word| match word.strip_prefix('!') {
                                Some(rest) => Pattern {
                                    text: rest.to_string(),
                                    negated: true,
                                },
                                None => Pattern {
                                    text: word,
                                    negated: false,
                                },
                            })
                            .collect(),
                    ),
                    options: Vec::new(),
                }),
                "match" => blocks.push(Block {
                    scope: Scope::Match,
                    options: Vec::new(),
                }),
                _ => {
                    if let Some(block) = blocks.last_mut() {
                        block.options.push((key, unquote(value)));
                    }
                }
            }
        }
        Self { blocks }
    }

    /// 配置中写明的别名（不含通配符、不是否定的），按出现的顺序。
    pub fn aliases(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for block in &self.blocks {
            if let Scope::Host(patterns) = &block.scope {
                for p in patterns {
                    let literal = !p.negated && !p.text.is_empty() && !p.text.contains(['*', '?']);
                    if literal && !out.iter().any(|a| a.eq_ignore_ascii_case(&p.text)) {
                        out.push(&p.text);
                    }
                }
            }
        }
        out
    }

    /// 配置中已经有这个名字的 Host（不区分大小写，与 ssh 匹配别名的规则相同）。
    pub fn has_alias(&self, name: &str) -> bool {
        self.aliases().iter().any(|a| a.eq_ignore_ascii_case(name))
    }

    /// `alias` 的某一项的值：第一个匹配的块中的。
    fn get(&self, alias: &str, key: &str) -> Option<&str> {
        self.blocks
            .iter()
            .filter(|block| applies(&block.scope, alias))
            .find_map(|block| {
                block
                    .options
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str())
            })
    }

    /// 连到 `address`（端口 `port`、用户 `user`）的别名：按配置算出的 HostName、Port、User 与之相同的第一个。
    /// 没有写 User 的按本机的用户名 `local_user`，没有写 Port 的按 22，没有写 HostName 的按别名本身。
    pub fn find_alias(
        &self,
        address: &str,
        port: u16,
        user: &str,
        local_user: &str,
    ) -> Option<String> {
        let address = unbracket(address);
        self.aliases().into_iter().find_map(|alias| {
            let host = match self.get(alias, "hostname") {
                Some(name) => expand_host(name, alias),
                None => alias.to_string(),
            };
            let alias_port = match self.get(alias, "port") {
                Some(text) => text.trim().parse::<u16>().ok()?,
                None => 22,
            };
            let alias_user = self.get(alias, "user").unwrap_or(local_user);
            (unbracket(&host).eq_ignore_ascii_case(address)
                && alias_port == port
                && alias_user == user)
                .then(|| alias.to_string())
        })
    }
}

/// `scope` 的块对 `alias` 生效：全局；或至少一个模式匹配、并且没有否定的模式匹配。
fn applies(scope: &Scope, alias: &str) -> bool {
    match scope {
        Scope::Global => true,
        Scope::Match => false,
        Scope::Host(patterns) => {
            let hit = |p: &Pattern| glob(&p.text.to_lowercase(), &alias.to_lowercase());
            patterns.iter().any(|p| !p.negated && hit(p))
                && !patterns.iter().any(|p| p.negated && hit(p))
        }
    }
}

/// ssh 的模式：`*` 任意个字符，`?` 一个字符。
fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// HostName 中的 `%h`（别名）与 `%%`。
fn expand_host(value: &str, alias: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(alias),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn unbracket(address: &str) -> &str {
    address.trim_start_matches('[').trim_end_matches(']')
}

/// 一行拆成键与值：`Key value`、`Key=value`、`Key = value`。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let end = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let key = &line[..end];
    let rest = line[end..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    (!key.is_empty()).then_some((key, rest))
}

/// 按空白拆开（双引号中的空白不拆）。
fn split_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in value.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
        .to_string()
}

// ---------------------------------------------------------------- 建议的条目

/// 建议追加到 `~/.ssh/config` 的 Host 条目。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 新的别名（`termlet-` 加主机名称，与配置中已有的不重复）
    pub alias: String,
    /// 要追加的文本（以注释行开头，以换行结尾）
    pub text: String,
}

impl Entry {
    /// 这台主机的条目：HostName、User、非 22 的 Port、本机的私钥文件（IdentityFile）与指定的证书文件（CertificateFile）、
    /// 跳板机（ProxyJump）。
    /// `stamp` 写在开头的注释中。
    pub fn for_endpoint(endpoint: &Endpoint, config: &SshConfig, stamp: &str) -> Self {
        let alias = unique_alias(&endpoint.name, &endpoint.address, config);
        let mut text = String::new();
        let label = one_line(&endpoint.name);
        if label.is_empty() {
            text.push_str(&format!("# Termlet {stamp}\n"));
        } else {
            text.push_str(&format!("# Termlet {stamp}: {label}\n"));
        }
        text.push_str(&format!("Host {alias}\n"));
        option(&mut text, "HostName", &endpoint.address);
        option(&mut text, "User", &endpoint.user);
        if endpoint.port != 22 {
            option(&mut text, "Port", &endpoint.port.to_string());
        }
        if let Some(path) = &endpoint.identity_file {
            option(&mut text, "IdentityFile", &path.to_string_lossy());
        }
        if let Some(path) = &endpoint.certificate_file {
            option(&mut text, "CertificateFile", &path.to_string_lossy());
        }
        if !endpoint.jumps.is_empty() {
            let hops: Vec<String> = endpoint
                .jumps
                .iter()
                .map(|hop| {
                    let address = super::bracket(&one_line(&hop.address));
                    let user = one_line(&hop.user);
                    let user = if user.is_empty() {
                        String::new()
                    } else {
                        format!("{user}@")
                    };
                    if hop.port == 22 {
                        format!("{user}{address}")
                    } else {
                        format!("{user}{address}:{}", hop.port)
                    }
                })
                .collect();
            option(&mut text, "ProxyJump", &hops.join(","));
        }
        Self { alias, text }
    }
}

/// `termlet-` 加主机名称（小写，字母、数字与 `._-` 以外的换成 `-`），名称中没有这些字符时用地址；与配置中已有的
/// 别名重复时加 `-2`、`-3`……
fn unique_alias(name: &str, address: &str, config: &SshConfig) -> String {
    let slug = |text: &str| -> String {
        let mut out = String::new();
        for c in text.trim().chars() {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                out.push(c);
            } else if !out.ends_with('-') {
                out.push('-');
            }
        }
        out.trim_matches(['-', '.']).to_string()
    };
    let mut base = slug(name);
    if base.is_empty() {
        base = slug(address);
    }
    if base.is_empty() {
        base = "host".to_string();
    }
    let base = format!("termlet-{base}");
    let mut alias = base.clone();
    let mut n = 2;
    while config.has_alias(&alias) {
        alias = format!("{base}-{n}");
        n += 1;
    }
    alias
}

/// 写一行配置（四个空格缩进），值为空时不写；含空白的值加引号。
fn option(out: &mut String, key: &str, value: &str) {
    let value = one_line(value);
    if value.is_empty() {
        return;
    }
    if value.contains(char::is_whitespace) {
        out.push_str(&format!("    {key} \"{value}\"\n"));
    } else {
        out.push_str(&format!("    {key} {value}\n"));
    }
}

/// 去掉换行等控制字符与引号（配置中的一行）。
fn one_line(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() && *c != '"')
        .collect::<String>()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------- 追加

/// `~/.ssh/config` 的位置；取不到主目录时为 `None`。
pub fn default_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh").join("config"))
}

/// 追加的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Appended {
    /// 原文件的备份；文件原来不存在（新建的）时为 `None`
    pub backup: Option<PathBuf>,
}

/// 把 `entry` 追加到 `path` 的末尾：先把原文件复制为 `<文件名>.termlet-backup-<suffix>`（已有同名的加 `-2`……），
/// 原文件不以换行结尾时先补一个，与前面的内容隔一个空行；原文件用 CRLF 换行时条目也用 CRLF。文件不存在时新建
/// （连同目录）。只追加，不改动已有的内容。
pub fn append(path: &Path, entry: &str, suffix: &str) -> io::Result<Appended> {
    let existing = match fs::read(path) {
        Ok(raw) => Some(raw),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let Some(existing) = existing else {
        if let Some(dir) = path.parent() {
            create_private_dir(dir)?;
        }
        let mut file = create_private_file(path)?;
        file.write_all(entry.as_bytes())?;
        file.sync_all()?;
        return Ok(Appended { backup: None });
    };
    let backup = backup_path(path, suffix);
    fs::copy(path, &backup)?;
    let crlf = existing.windows(2).any(|w| w == b"\r\n");
    let newline = if crlf { "\r\n" } else { "\n" };
    let mut text = String::new();
    if !existing.is_empty() {
        if !existing.ends_with(b"\n") {
            text.push_str(newline);
        }
        text.push_str(newline);
    }
    if crlf {
        text.push_str(&entry.replace("\r\n", "\n").replace('\n', "\r\n"));
    } else {
        text.push_str(entry);
    }
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    Ok(Appended {
        backup: Some(backup),
    })
}

/// [`append`]，备份的后缀为现在的本地时间（`20261109-180000`）。
pub fn append_now(path: &Path, entry: &str) -> io::Result<Appended> {
    let suffix = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    append(path, entry, &suffix)
}

/// 备份的位置：同一目录下的 `<文件名>.termlet-backup-<suffix>`，已有时加 `-2`、`-3`……
fn backup_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config".into());
    let base = path.with_file_name(format!("{name}.termlet-backup-{suffix}"));
    let mut candidate = base.clone();
    let mut n = 2;
    while candidate.exists() {
        candidate = PathBuf::from(format!("{}-{n}", base.display()));
        n += 1;
    }
    candidate
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    if dir.exists() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
