//! X11 转发（OpenSSH 的 `ForwardX11`）：服务器上的图形程序显示在本机的 X 服务器上。
//!
//! 过程与 OpenSSH 相同：
//! 1. 终端请求转发（`x11-req`）时发给服务器的是**伪造的随机 cookie**（16 字节，MIT-MAGIC-COOKIE-1）；服务器把它写进
//!    远端的 xauth，设好 `DISPLAY`（如 `localhost:10.0`）。
//! 2. 远端的 X 程序连接时，服务器开一个 `x11` 通道送来。先读出 X 协议的连接请求，**校验其中的 cookie 就是伪造的那个**
//!    （不是的关闭：只接受经过 sshd 的程序），再**换成本机的真实 cookie**（没有时不带认证）连接本机的 X 服务器。
//!    真实的 cookie 不离开本机。
//! 3. 真实的 cookie：受信任（`ssh -Y`，`ForwardX11Trusted yes`）时取本机 Xauthority 文件中这个显示的
//!    （`XAUTHORITY`，默认 `~/.Xauthority`；Windows 上的 X 服务器多半不用认证，没有时不带）；
//!    不受信任（`ssh -X`，默认，与 OpenSSH 上游的 `ForwardX11Trusted no` 相同）时用本机的 xauth 向 X 服务器申请一个
//!    受限的 cookie（`xauth generate … untrusted`：X 服务器的 SECURITY 扩展据此限制这些程序，不能读别的窗口的内容与
//!    按键）。本机没有 xauth、X 服务器不支持时与 OpenSSH 一样不转发（终端中说明，可以改用受信任）。申请到的 cookie
//!    没有程序在用超过 20 分钟就被 X 服务器作废，之后的程序连接时重新申请。
//!
//! 本机的 X 显示（[`Display::parse`]）：`localhost:0`（TCP 6000，Windows 上 VcXsrv、X410 的默认）、`主机:N`
//! （TCP 6000+N）、`:N` / `unix:N`（Unix 上为 `/tmp/.X11-unix/XN`，Windows 上按 `localhost:N`）、`/路径:N`
//! （macOS 上 XQuartz 的 launchd 套接字），可以带屏幕号（`:0.1`）。本机没有 X 服务器时连接照常，第一个 X 程序打开
//! 窗口时说明（每条连接一次，[`X11Notice::NoServer`]）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use russh::Channel;
use russh::client::Msg;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

/// 没有设置、也没有 `DISPLAY` 时用的本机 X 显示。
pub const DEFAULT_DISPLAY: &str = "localhost:0";
/// 唯一支持的认证协议。
const PROTOCOL: &str = "MIT-MAGIC-COOKIE-1";
const COOKIE_LEN: usize = 16;
/// 连接本机 X 服务器的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// 等远端程序发来连接请求的最长时间。
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
/// 连接请求最长多少字节（认证协议名与数据各最多 64 KiB，实际都很短）。
const MAX_SETUP: usize = 16 * 1024;
/// 不受信任的 cookie 没有程序使用多久后被 X 服务器作废（与 OpenSSH 的 ForwardX11Timeout 默认值相同）。
const UNTRUSTED_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// 运行 xauth 的最长时间。
const XAUTH_TIMEOUT: Duration = Duration::from_secs(10);

/// 一个终端的 X11 转发设置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct X11Options {
    /// 本机的 X 显示（[`Display::parse`] 的写法）
    pub display: String,
    /// 受信任（`ssh -Y`）
    pub trusted: bool,
}

impl X11Options {
    /// 本机设置中的显示：没有设置时用 `DISPLAY`，再没有时用 [`DEFAULT_DISPLAY`]。
    pub fn local_display(configured: &str) -> String {
        let configured = configured.trim();
        if !configured.is_empty() {
            return configured.to_string();
        }
        std::env::var("DISPLAY")
            .ok()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| DEFAULT_DISPLAY.to_string())
    }
}

/// 本机的 X 显示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Display {
    pub target: DisplayTarget,
    /// 显示号
    pub number: u32,
    /// 屏幕号
    pub screen: u32,
    /// 原文（交给 xauth）
    pub text: String,
}

/// X 服务器在哪里监听。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisplayTarget {
    Tcp {
        host: String,
        port: u16,
    },
    /// Unix 套接字
    Unix(PathBuf),
}

impl Display {
    /// 解析 X 显示的写法；不认识时为 `None`。
    pub fn parse(text: &str) -> Option<Display> {
        let text = text.trim();
        let (host, rest) = text.rsplit_once(':')?;
        let (number, screen) = match rest.split_once('.') {
            Some((n, s)) => (n.parse::<u32>().ok()?, s.parse::<u32>().ok()?),
            None => (rest.parse::<u32>().ok()?, 0),
        };
        let tcp = |host: &str| {
            let port = u16::try_from(6000 + u64::from(number)).ok()?;
            Some(DisplayTarget::Tcp {
                host: host.to_string(),
                port,
            })
        };
        let target = if host.starts_with('/') {
            // macOS 的 launchd：`/private/tmp/com.apple.launchd.xxx/org.xquartz:0`，冒号之前是套接字
            if cfg!(windows) {
                return None;
            }
            DisplayTarget::Unix(PathBuf::from(host))
        } else if host.is_empty() || host == "unix" {
            if cfg!(windows) {
                tcp("localhost")?
            } else {
                DisplayTarget::Unix(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
            }
        } else {
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            if host.contains(['/', ' ']) {
                return None;
            }
            tcp(host)?
        };
        Some(Display {
            target,
            number,
            screen,
            text: text.to_string(),
        })
    }

    /// 是否是本机的显示（Xauthority 中按"本机"一类的条目查找）。
    fn is_local(&self) -> bool {
        match &self.target {
            DisplayTarget::Unix(_) => true,
            DisplayTarget::Tcp { host, .. } => {
                host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
            }
        }
    }
}

/// 本机的真实认证（换进连接请求中的）。
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct LocalAuth {
    pub name: String,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for LocalAuth {
    // cookie 是秘密：不进 Debug 输出
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalAuth")
            .field("name", &self.name)
            .field("data", &format_args!("<{} 字节>", self.data.len()))
            .finish()
    }
}

// ---------------------------------------------------------------- Xauthority

/// Xauthority 中的地址族。
const FAMILY_INTERNET: u16 = 0;
const FAMILY_INTERNET6: u16 = 6;
const FAMILY_LOCAL: u16 = 256;
const FAMILY_WILD: u16 = 65535;

#[derive(Debug, PartialEq, Eq)]
struct XauthEntry {
    family: u16,
    address: Vec<u8>,
    number: String,
    name: String,
    data: Vec<u8>,
}

/// 解析 Xauthority 文件（每条：地址族 u16、地址、显示号、协议名、数据，后四项都是 u16 长度加内容，大端）。
/// 后面不完整的部分忽略。
fn parse_xauthority(raw: &[u8]) -> Vec<XauthEntry> {
    fn field<'a>(raw: &'a [u8], at: &mut usize) -> Option<&'a [u8]> {
        let len = u16::from_be_bytes(raw.get(*at..*at + 2)?.try_into().ok()?) as usize;
        let value = raw.get(*at + 2..*at + 2 + len)?;
        *at += 2 + len;
        Some(value)
    }
    let mut entries = Vec::new();
    let mut at = 0;
    while let Some(family) = raw
        .get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
    {
        at += 2;
        let (Some(address), Some(number), Some(name), Some(data)) = (
            field(raw, &mut at),
            field(raw, &mut at),
            field(raw, &mut at),
            field(raw, &mut at),
        ) else {
            break;
        };
        entries.push(XauthEntry {
            family,
            address: address.to_vec(),
            number: String::from_utf8_lossy(number).into_owned(),
            name: String::from_utf8_lossy(name).into_owned(),
            data: data.to_vec(),
        });
    }
    entries
}

/// 在 Xauthority 的条目中找这个显示的 MIT-MAGIC-COOKIE-1（与 Xlib 的 XauGetBestAuthByAddr 一样取第一个匹配的）。
/// `hostname` 是本机的主机名（"本机"一类条目的地址）；不知道时接受任何"本机"条目。
fn find_cookie(
    entries: &[XauthEntry],
    display: &Display,
    hostname: Option<&str>,
) -> Option<LocalAuth> {
    let number = display.number.to_string();
    let ip = match &display.target {
        DisplayTarget::Tcp { host, .. } => host.parse::<std::net::IpAddr>().ok(),
        DisplayTarget::Unix(_) => None,
    };
    entries
        .iter()
        .filter(|e| e.name == PROTOCOL && (e.number.is_empty() || e.number == number))
        .find(|e| match e.family {
            FAMILY_WILD => true,
            FAMILY_LOCAL => {
                display.is_local()
                    && hostname.is_none_or(|h| e.address.eq_ignore_ascii_case(h.as_bytes()))
            }
            FAMILY_INTERNET => match ip {
                Some(std::net::IpAddr::V4(ip)) => e.address == ip.octets(),
                _ => display.is_local() && e.address == [127, 0, 0, 1],
            },
            FAMILY_INTERNET6 => {
                matches!(ip, Some(std::net::IpAddr::V6(ip)) if e.address == ip.octets())
            }
            _ => false,
        })
        .map(|e| LocalAuth {
            name: e.name.clone(),
            data: e.data.clone(),
        })
}

/// 本机的 Xauthority 文件：`XAUTHORITY`，没有时为家目录中的 `.Xauthority`。
fn xauthority_path() -> Option<PathBuf> {
    std::env::var_os("XAUTHORITY")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".Xauthority")))
}

/// 本机的主机名（Xauthority 中"本机"一类条目的地址）。
fn hostname() -> Option<String> {
    #[cfg(windows)]
    {
        std::env::var("COMPUTERNAME").ok().filter(|h| !h.is_empty())
    }
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        // SAFETY：缓冲区有效，长度正确；结果按 NUL 截断
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        (ok && end > 0).then(|| String::from_utf8_lossy(&buf[..end]).into_owned())
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

/// 受信任时的真实 cookie：Xauthority（`path`，没有时为默认的位置）中这个显示的；没有文件、没有条目时为 `None`
/// （不带认证连接）。
fn trusted_cookie(display: &Display, path: Option<PathBuf>) -> Option<LocalAuth> {
    let path = path.or_else(xauthority_path)?;
    let raw = std::fs::read(&path).ok()?;
    let found = find_cookie(&parse_xauthority(&raw), display, hostname().as_deref());
    let shown = display.text.as_str();
    tracing::debug!(
        x_display = shown,
        found = found.is_some(),
        "在 Xauthority 中查找 X 显示的 cookie"
    );
    found
}

// ---------------------------------------------------------------- 不受信任：xauth

/// 准备转发时出的问题（这时不请求转发）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11Problem {
    /// 本机 X 显示的写法不对
    BadDisplay(String),
    /// 不受信任需要本机的 xauth，没有找到
    NoXauth,
    /// xauth 没能申请到不受信任的 cookie（X 服务器没有运行、不支持 SECURITY 扩展等）
    XauthFailed(String),
}

impl std::fmt::Display for X11Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            X11Problem::BadDisplay(d) => write!(f, "本机 X 显示的写法不对：{d}"),
            X11Problem::NoXauth => f.write_str("不受信任的 X11 转发需要本机的 xauth，没有找到"),
            X11Problem::XauthFailed(reason) => {
                write!(f, "xauth 没能生成不受信任的 cookie：{reason}")
            }
        }
    }
}

/// 本机的 xauth：PATH 中的，或常见的安装位置。
fn find_xauth() -> Option<PathBuf> {
    let exe = if cfg!(windows) { "xauth.exe" } else { "xauth" };
    let in_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(exe));
    let known: Vec<PathBuf> = if cfg!(windows) {
        ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|p| PathBuf::from(p).join("VcXsrv").join(exe))
            .collect()
    } else {
        [
            "/usr/bin/xauth",
            "/opt/X11/bin/xauth",
            "/usr/X11R6/bin/xauth",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    };
    in_path.chain(known).find(|p| p.is_file())
}

/// `xauth list` 输出中的一行：`显示  协议名  十六进制数据`。
fn parse_xauth_list(output: &str) -> Option<LocalAuth> {
    output.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        let _display = words.next()?;
        let name = words.next()?;
        let data = data_encoding::HEXLOWER_PERMISSIVE
            .decode(words.next()?.as_bytes())
            .ok()?;
        (name == PROTOCOL && !data.is_empty()).then(|| LocalAuth {
            name: name.to_string(),
            data,
        })
    })
}

/// 用 xauth 向 X 服务器申请一个不受信任的 cookie（与 OpenSSH 的做法相同：写进一个临时文件，再从中读出）。
async fn untrusted_cookie(display: &Display) -> Result<LocalAuth, X11Problem> {
    let xauth = find_xauth().ok_or(X11Problem::NoXauth)?;
    let dir = std::env::temp_dir().join(format!("termlet-xauth-{}", crate::id::random_token(8)));
    std::fs::create_dir(&dir).map_err(|e| X11Problem::XauthFailed(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let file = dir.join("xauthfile");
    let result = run_xauth(&xauth, &file, display).await;
    let _ = std::fs::remove_dir_all(&dir);
    result
}

async fn run_xauth(xauth: &Path, file: &Path, display: &Display) -> Result<LocalAuth, X11Problem> {
    let run = |args: Vec<String>| {
        let mut cmd = tokio::process::Command::new(xauth);
        cmd.arg("-f")
            .arg(file)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        async move {
            match tokio::time::timeout(XAUTH_TIMEOUT, cmd.output()).await {
                Ok(Ok(output)) => Ok(output),
                Ok(Err(e)) => Err(X11Problem::XauthFailed(e.to_string())),
                Err(_) => Err(X11Problem::XauthFailed("xauth 超时".into())),
            }
        }
    };
    // timeout 比作废时间多 60 秒，与 OpenSSH 相同
    let generated = run(vec![
        "generate".into(),
        display.text.clone(),
        PROTOCOL.into(),
        "untrusted".into(),
        "timeout".into(),
        (UNTRUSTED_TIMEOUT.as_secs() + 60).to_string(),
    ])
    .await?;
    if !generated.status.success() {
        let reason = String::from_utf8_lossy(&generated.stderr)
            .trim()
            .to_string();
        return Err(X11Problem::XauthFailed(if reason.is_empty() {
            format!("xauth 退出码 {:?}", generated.status.code())
        } else {
            reason
        }));
    }
    let listed = run(vec!["list".into(), display.text.clone()]).await?;
    parse_xauth_list(&String::from_utf8_lossy(&listed.stdout))
        .ok_or_else(|| X11Problem::XauthFailed("xauth 没有给出 cookie".into()))
}

// ---------------------------------------------------------------- 连接请求

/// 连接请求不能转发的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupError {
    /// 不是 X11 的连接请求
    Malformed,
    /// 认证协议不是 MIT-MAGIC-COOKIE-1，或 cookie 不是发给服务器的那个
    WrongCookie,
}

fn pad4(n: usize) -> usize {
    (4 - n % 4) % 4
}

/// 读出远端程序发来的连接请求，校验 cookie 是伪造的那个，换成本机的认证（`None` 时不带认证）。
/// `buf` 还不完整时返回 `Ok(None)`；完整时返回改写后的连接请求与它在 `buf` 中占的字节数。
fn rewrite_setup(
    buf: &[u8],
    fake: &[u8],
    real: Option<&LocalAuth>,
) -> Result<Option<(Vec<u8>, usize)>, SetupError> {
    if buf.len() < 12 {
        return Ok(None);
    }
    let read_u16 = |at: usize| -> u16 {
        let bytes = [buf[at], buf[at + 1]];
        match buf[0] {
            b'B' => u16::from_be_bytes(bytes),
            _ => u16::from_le_bytes(bytes),
        }
    };
    if buf[0] != b'B' && buf[0] != b'l' {
        return Err(SetupError::Malformed);
    }
    let name_len = read_u16(6) as usize;
    let data_len = read_u16(8) as usize;
    let name_at = 12;
    let data_at = name_at + name_len + pad4(name_len);
    let total = data_at + data_len + pad4(data_len);
    if total > MAX_SETUP {
        return Err(SetupError::Malformed);
    }
    if buf.len() < total {
        return Ok(None);
    }
    let name = &buf[name_at..name_at + name_len];
    let data = &buf[data_at..data_at + data_len];
    if name != PROTOCOL.as_bytes() || !constant_time_eq(data, fake) {
        return Err(SetupError::WrongCookie);
    }
    let (new_name, new_data): (&[u8], &[u8]) = match real {
        Some(auth) => (auth.name.as_bytes(), &auth.data),
        None => (&[], &[]),
    };
    let write_u16 = |value: usize| -> [u8; 2] {
        let value = value as u16;
        match buf[0] {
            b'B' => value.to_be_bytes(),
            _ => value.to_le_bytes(),
        }
    };
    let mut out = Vec::with_capacity(12 + new_name.len() + new_data.len() + 6);
    out.extend_from_slice(&buf[..6]);
    out.extend_from_slice(&write_u16(new_name.len()));
    out.extend_from_slice(&write_u16(new_data.len()));
    out.extend_from_slice(&buf[10..12]);
    out.extend_from_slice(new_name);
    out.resize(out.len() + pad4(new_name.len()), 0);
    out.extend_from_slice(new_data);
    out.resize(out.len() + pad4(new_data.len()), 0);
    Ok(Some((out, total)))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------- 一条连接上的转发

/// 转发进行中的说明（界面在终端中显示）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11Notice {
    /// 连不上本机的 X 服务器（每条连接只报一次）
    NoServer { display: String, reason: String },
    /// 不受信任的 cookie 过期后没能重新申请
    CookieFailed(X11Problem),
}

/// 请求转发的结果（打开终端时）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum X11Status {
    /// 主机没有开启
    Off,
    /// 已请求，服务器同意（或没有回答）
    Forwarding { display: String, trusted: bool },
    /// 服务器拒绝（sshd 的 `X11Forwarding no`，或服务器上没有 xauth）
    Refused,
    /// 本机准备不了，没有请求
    Unavailable(X11Problem),
}

/// 发给服务器的 `x11-req` 的内容。
pub(crate) struct X11Request {
    pub protocol: &'static str,
    /// 伪造的 cookie（十六进制）
    pub cookie: String,
    pub screen: u32,
}

struct Session {
    fake: [u8; COOKIE_LEN],
    display: Display,
    trusted: bool,
    /// 本机的真实认证；不受信任时是 xauth 申请到的
    auth: Option<LocalAuth>,
    /// 正在转发的 X 连接数（不受信任的 cookie 在没有连接时才会过期）
    active: usize,
    /// 不受信任的 cookie 何时过期（没有连接后开始计时）
    expires: Instant,
}

/// 一条 SSH 连接上的 X11 转发：终端请求转发前准备好（[`X11Forwarding::prepare`]），之后接受服务器打开的 x11 通道。
/// 没有请求过的连接拒绝 x11 通道。
pub(crate) struct X11Forwarding {
    session: Mutex<Option<Session>>,
    notices: watch::Sender<Option<(u64, X11Notice)>>,
    next_notice: AtomicU64,
    /// 报过"连不上 X 服务器"
    reported_no_server: AtomicBool,
    /// 显示用的主机名称（日志）
    host: String,
    /// 测试中指定的 Xauthority 文件（不改进程的环境变量）
    xauthority: Mutex<Option<PathBuf>>,
}

impl X11Forwarding {
    pub(crate) fn new(host: &str) -> Self {
        Self {
            session: Mutex::new(None),
            notices: watch::channel(None).0,
            next_notice: AtomicU64::new(0),
            reported_no_server: AtomicBool::new(false),
            host: host.to_string(),
            xauthority: Mutex::new(None),
        }
    }

    /// 测试用：受信任时从这个文件取本机的 cookie。
    #[cfg(test)]
    pub(crate) fn use_xauthority(&self, path: PathBuf) {
        *self.xauthority.lock().unwrap() = Some(path);
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 转发中的说明；订阅之后的才收到。
    pub(crate) fn notices(&self) -> watch::Receiver<Option<(u64, X11Notice)>> {
        self.notices.subscribe()
    }

    fn notify(&self, notice: X11Notice) {
        let seq = self.next_notice.fetch_add(1, Ordering::Relaxed);
        self.notices.send_replace(Some((seq, notice)));
    }

    /// 请求转发前准备：解析本机的显示，取得（或申请）本机的真实认证。同一条连接上的终端共用一个伪造的 cookie
    /// （与 OpenSSH 相同）；设置变了时换成新的显示与认证。
    pub(crate) async fn prepare(&self, options: &X11Options) -> Result<X11Request, X11Problem> {
        let display = Display::parse(&options.display)
            .ok_or_else(|| X11Problem::BadDisplay(options.display.clone()))?;
        let auth = if options.trusted {
            let display = display.clone();
            let path = self
                .xauthority
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            tokio::task::spawn_blocking(move || trusted_cookie(&display, path))
                .await
                .ok()
                .flatten()
        } else {
            Some(untrusted_cookie(&display).await?)
        };
        let mut slot = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let fake = match slot.as_ref() {
            Some(session) => session.fake,
            None => {
                let mut fake = [0u8; COOKIE_LEN];
                getrandom::fill(&mut fake).expect("系统随机数不可用");
                fake
            }
        };
        let screen = display.screen;
        let shown = display.text.as_str();
        tracing::info!(
            host = %self.host,
            x_display = shown,
            trusted = options.trusted,
            local_auth = auth.is_some(),
            "准备 X11 转发"
        );
        let active = slot.as_ref().map_or(0, |s| s.active);
        *slot = Some(Session {
            fake,
            display,
            trusted: options.trusted,
            auth,
            active,
            expires: Instant::now() + UNTRUSTED_TIMEOUT,
        });
        Ok(X11Request {
            protocol: PROTOCOL,
            cookie: data_encoding::HEXLOWER.encode(&fake),
            screen,
        })
    }

    /// 服务器打开了一个 x11 通道（已接受）：在后台校验、换 cookie、连接本机的 X 服务器并转发。
    pub(crate) fn serve(self: &std::sync::Arc<Self>, channel: Channel<Msg>, originator: String) {
        let this = self.clone();
        crate::runtime::spawn(async move {
            let mut remote = channel.into_stream();
            match this.forward(&mut remote).await {
                Ok(()) => {
                    tracing::debug!(host = %this.host, originator = %originator, "X11 连接结束")
                }
                Err(reason) => {
                    tracing::info!(host = %this.host, originator = %originator, "X11 连接没有转发：{reason}")
                }
            }
            let _ = remote.shutdown().await;
        });
    }

    async fn forward<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        remote: &mut S,
    ) -> Result<(), String> {
        // 1. 读出连接请求
        let mut buf = Vec::with_capacity(64);
        let fake = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| s.fake)
            .ok_or("没有请求过 X11 转发")?;
        let read = async {
            loop {
                match rewrite_setup(&buf, &fake, None) {
                    Ok(None) => {}
                    Ok(Some(_)) => return Ok(()),
                    Err(e) => return Err(e),
                }
                let mut chunk = [0u8; 1024];
                let n = remote
                    .read(&mut chunk)
                    .await
                    .map_err(|_| SetupError::Malformed)?;
                if n == 0 {
                    return Err(SetupError::Malformed);
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        };
        match tokio::time::timeout(SETUP_TIMEOUT, read).await {
            Ok(Ok(())) => {}
            Ok(Err(SetupError::WrongCookie)) => {
                tracing::warn!(host = %self.host, "X11 连接带的 cookie 不对（不是经过 sshd 的程序），拒绝");
                return Err("cookie 不对".into());
            }
            Ok(Err(SetupError::Malformed)) => return Err("不是 X11 的连接请求".into()),
            Err(_) => return Err("等待连接请求超时".into()),
        }
        // 2. 取本机的认证（不受信任的过期了就重新申请）
        let (display, auth) = self.local_auth().await.map_err(|problem| {
            let reason = problem.to_string();
            self.notify(X11Notice::CookieFailed(problem));
            reason
        })?;
        let (setup, used) = rewrite_setup(&buf, &fake, auth.as_ref())
            .ok()
            .flatten()
            .ok_or("连接请求无法改写")?;
        // 3. 连接本机的 X 服务器
        let local = match tokio::time::timeout(CONNECT_TIMEOUT, connect_local(&display)).await {
            Ok(Ok(local)) => local,
            Ok(Err(e)) => return Err(self.no_server(&display, e.to_string())),
            Err(_) => return Err(self.no_server(&display, "连接超时".into())),
        };
        self.reported_no_server.store(false, Ordering::Relaxed);
        self.track(1);
        let result = pump(local, remote, &setup, &buf[used..]).await;
        self.track(-1);
        result.map_err(|e| e.to_string())
    }

    /// 本机的显示与认证；不受信任的 cookie 没有连接使用超过作废时间时重新申请。
    async fn local_auth(&self) -> Result<(Display, Option<LocalAuth>), X11Problem> {
        let (display, auth, refresh) = {
            let slot = self.session.lock().unwrap_or_else(|e| e.into_inner());
            let session = slot.as_ref().ok_or(X11Problem::NoXauth)?;
            let refresh = !session.trusted
                && session.active == 0
                && Instant::now() + Duration::from_secs(60) > session.expires;
            (session.display.clone(), session.auth.clone(), refresh)
        };
        if !refresh {
            return Ok((display, auth));
        }
        tracing::info!(host = %self.host, "不受信任的 X11 cookie 已过期，重新申请");
        let fresh = untrusted_cookie(&display).await?;
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            session.auth = Some(fresh.clone());
            session.expires = Instant::now() + UNTRUSTED_TIMEOUT;
        }
        Ok((display, Some(fresh)))
    }

    fn track(&self, delta: isize) {
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            session.active = session.active.saturating_add_signed(delta);
            if session.active == 0 {
                session.expires = Instant::now() + UNTRUSTED_TIMEOUT;
            }
        }
    }

    /// 连不上本机的 X 服务器：每条连接只说明一次（之后连上过再断开时重新说明）。
    fn no_server(&self, display: &Display, reason: String) -> String {
        let shown = display.text.as_str();
        tracing::info!(host = %self.host, x_display = shown, "连不上本机的 X 服务器：{reason}");
        if !self.reported_no_server.swap(true, Ordering::Relaxed) {
            self.notify(X11Notice::NoServer {
                display: display.text.clone(),
                reason: reason.clone(),
            });
        }
        reason
    }
}

/// 双向的字节流（本机 X 服务器的连接）。
trait LocalStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> LocalStream for T {}

async fn connect_local(display: &Display) -> std::io::Result<Box<dyn LocalStream>> {
    match &display.target {
        DisplayTarget::Tcp { host, port } => {
            // localhost：X 服务器多半只监听 IPv4，并发连接不用等 ::1 失败（Windows 上要约 2 秒）
            let stream = super::dial::connect_host(host, *port, CONNECT_TIMEOUT).await?;
            let _ = stream.set_nodelay(true);
            Ok(Box::new(stream))
        }
        #[cfg(unix)]
        DisplayTarget::Unix(path) => Ok(Box::new(tokio::net::UnixStream::connect(path).await?)),
        #[cfg(not(unix))]
        DisplayTarget::Unix(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "这个系统不支持 Unix 套接字的 X 显示",
        )),
    }
}

/// 先把改写后的连接请求（与之后已经收到的字节）发给本机，再双向转发到任一方关闭。
async fn pump<S: AsyncRead + AsyncWrite + Unpin>(
    mut local: Box<dyn LocalStream>,
    remote: &mut S,
    setup: &[u8],
    rest: &[u8],
) -> std::io::Result<()> {
    local.write_all(setup).await?;
    if !rest.is_empty() {
        local.write_all(rest).await?;
    }
    tokio::io::copy_bidirectional(&mut local, remote).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_display_names() {
        let tcp = |host: &str, port| DisplayTarget::Tcp {
            host: host.into(),
            port,
        };
        let d = Display::parse("localhost:0").unwrap();
        assert_eq!(
            (d.target, d.number, d.screen),
            (tcp("localhost", 6000), 0, 0)
        );
        let d = Display::parse(" 192.168.1.5:10.1 ").unwrap();
        assert_eq!(
            (d.target, d.number, d.screen),
            (tcp("192.168.1.5", 6010), 10, 1)
        );
        assert_eq!(d.text, "192.168.1.5:10.1");
        let d = Display::parse("[::1]:2").unwrap();
        assert_eq!(d.target, tcp("::1", 6002));
        let d = Display::parse(":1").unwrap();
        if cfg!(windows) {
            assert_eq!(d.target, tcp("localhost", 6001));
            assert!(Display::parse("/tmp/launch-x/org.xquartz:0").is_none());
        } else {
            assert_eq!(d.target, DisplayTarget::Unix("/tmp/.X11-unix/X1".into()));
            let d = Display::parse("/private/tmp/com.apple.launchd.ab/org.xquartz:0").unwrap();
            assert_eq!(
                d.target,
                DisplayTarget::Unix("/private/tmp/com.apple.launchd.ab/org.xquartz".into())
            );
        }
        for bad in ["", "localhost", "localhost:x", "a b:0", "h:0.x", "h:99999"] {
            assert!(Display::parse(bad).is_none(), "{bad}");
        }
    }

    fn entry(family: u16, address: &[u8], number: &str, data: &[u8]) -> Vec<u8> {
        let mut out = family.to_be_bytes().to_vec();
        for field in [address, number.as_bytes(), PROTOCOL.as_bytes(), data] {
            out.extend_from_slice(&(field.len() as u16).to_be_bytes());
            out.extend_from_slice(field);
        }
        out
    }

    #[test]
    fn finds_the_cookie_for_a_display_in_xauthority() {
        let mut raw = entry(FAMILY_LOCAL, b"other-pc", "0", &[1; 16]);
        raw.extend(entry(FAMILY_LOCAL, b"my-pc", "1", &[2; 16]));
        raw.extend(entry(FAMILY_LOCAL, b"my-pc", "0", &[3; 16]));
        raw.extend(entry(FAMILY_INTERNET, &[10, 0, 0, 9], "0", &[4; 16]));
        raw.extend(entry(FAMILY_WILD, b"", "5", &[5; 16]));
        raw.extend_from_slice(&[0, 1, 0]); // 不完整的尾部
        let entries = parse_xauthority(&raw);
        assert_eq!(entries.len(), 5);
        let cookie = |display: &str, host: Option<&str>| {
            find_cookie(&entries, &Display::parse(display).unwrap(), host).map(|a| a.data[0])
        };
        assert_eq!(cookie("localhost:0", Some("my-pc")), Some(3));
        assert_eq!(cookie("localhost:1", Some("MY-PC")), Some(2));
        assert_eq!(
            cookie("localhost:0", None),
            Some(1),
            "不知道主机名时取第一个本机条目"
        );
        assert_eq!(cookie("10.0.0.9:0", Some("my-pc")), Some(4));
        assert_eq!(cookie("10.0.0.8:0", Some("my-pc")), None);
        assert_eq!(
            cookie("10.0.0.8:5", Some("my-pc")),
            Some(5),
            "任意地址的条目"
        );
        assert_eq!(cookie("localhost:7", Some("my-pc")), None);
    }

    #[test]
    fn parses_xauth_list_output() {
        let out = "my-pc/unix:0  MIT-MAGIC-COOKIE-1  00112233445566778899aabbccddeeff\n";
        let auth = parse_xauth_list(out).unwrap();
        assert_eq!(auth.name, PROTOCOL);
        assert_eq!(auth.data[0], 0x00);
        assert_eq!(auth.data[15], 0xff);
        assert!(parse_xauth_list("x XDM-AUTHORIZATION-1 0011\n").is_none());
        assert!(parse_xauth_list("").is_none());
        assert!(
            !format!("{auth:?}").contains("00112233"),
            "cookie 不进 Debug 输出"
        );
    }

    /// X 程序发来的连接请求（小端）。
    fn setup(name: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = vec![b'l', 0, 11, 0, 0, 0];
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name);
        out.resize(out.len() + pad4(name.len()), 0);
        out.extend_from_slice(data);
        out.resize(out.len() + pad4(data.len()), 0);
        out
    }

    #[test]
    fn checks_the_fake_cookie_and_swaps_in_the_real_one() {
        let fake = [7u8; 16];
        let request = setup(PROTOCOL.as_bytes(), &fake);
        // 还不完整
        assert_eq!(rewrite_setup(&request[..20], &fake, None), Ok(None));
        let real = LocalAuth {
            name: PROTOCOL.into(),
            data: vec![9; 16],
        };
        let mut with_rest = request.clone();
        with_rest.extend_from_slice(b"rest");
        let (rewritten, used) = rewrite_setup(&with_rest, &fake, Some(&real))
            .unwrap()
            .unwrap();
        assert_eq!(used, request.len());
        assert_eq!(rewritten, setup(PROTOCOL.as_bytes(), &[9; 16]));
        // 本机没有 cookie：不带认证
        let (bare, _) = rewrite_setup(&request, &fake, None).unwrap().unwrap();
        assert_eq!(bare, setup(b"", b""));
        // 大端的请求同样改写
        let mut big = request.clone();
        big[0] = b'B';
        big[6..8].copy_from_slice(&(PROTOCOL.len() as u16).to_be_bytes());
        big[8..10].copy_from_slice(&16u16.to_be_bytes());
        let (rewritten, _) = rewrite_setup(&big, &fake, Some(&real)).unwrap().unwrap();
        assert_eq!(&rewritten[8..10], &16u16.to_be_bytes());
        assert_eq!(&rewritten[rewritten.len() - 16..], &[9; 16]);
        // cookie 不对、协议不对、不是 X11：拒绝
        assert_eq!(
            rewrite_setup(&setup(PROTOCOL.as_bytes(), &[8; 16]), &fake, None),
            Err(SetupError::WrongCookie)
        );
        assert_eq!(
            rewrite_setup(&setup(b"XDM-AUTHORIZATION-1", &fake), &fake, None),
            Err(SetupError::WrongCookie)
        );
        assert_eq!(
            rewrite_setup(&setup(PROTOCOL.as_bytes(), &fake[..8]), &fake, None),
            Err(SetupError::WrongCookie)
        );
        assert_eq!(
            rewrite_setup(b"GET / HTTP/1.1\r\n", &fake, None),
            Err(SetupError::Malformed)
        );
    }

    // 同一条连接上的终端共用伪造的 cookie；受信任时真实的 cookie 来自 Xauthority（这里没有时为不带认证）
    #[test]
    fn prepare_keeps_the_fake_cookie_per_connection() {
        crate::runtime::runtime().block_on(async {
            let x11 = X11Forwarding::new("web");
            assert!(!x11.is_enabled());
            let options = X11Options {
                display: "localhost:3.1".into(),
                trusted: true,
            };
            let first = x11.prepare(&options).await.unwrap();
            assert!(x11.is_enabled());
            assert_eq!(first.protocol, PROTOCOL);
            assert_eq!(first.cookie.len(), 32);
            assert_eq!(first.screen, 1);
            let second = x11.prepare(&options).await.unwrap();
            assert_eq!(first.cookie, second.cookie);
            assert_eq!(
                X11Forwarding::new("db")
                    .prepare(&options)
                    .await
                    .unwrap()
                    .cookie
                    .len(),
                32
            );
            let bad = X11Options {
                display: "nowhere".into(),
                trusted: true,
            };
            assert_eq!(
                x11.prepare(&bad).await.err(),
                Some(X11Problem::BadDisplay("nowhere".into()))
            );
        });
    }

    // 本机没有 X 服务器：关闭这个 X 连接并说明一次；cookie 不对的不转发、不说明
    #[test]
    fn reports_a_missing_x_server_once() {
        crate::runtime::runtime().block_on(async {
            // 找一个没有监听的端口当作显示
            let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = free.local_addr().unwrap().port();
            drop(free);
            let Some(number) = port.checked_sub(6000) else {
                return;
            };
            let x11 = X11Forwarding::new("web");
            let request = x11
                .prepare(&X11Options {
                    display: format!("127.0.0.1:{number}"),
                    trusted: true,
                })
                .await
                .unwrap();
            let fake = data_encoding::HEXLOWER
                .decode(request.cookie.as_bytes())
                .unwrap();
            let mut notices = x11.notices();
            for _ in 0..2 {
                let (mut client, mut server) = tokio::io::duplex(1024);
                client
                    .write_all(&setup(PROTOCOL.as_bytes(), &fake))
                    .await
                    .unwrap();
                assert!(x11.forward(&mut server).await.is_err());
            }
            notices.changed().await.unwrap();
            let notice = notices.borrow_and_update().clone().unwrap().1;
            assert!(matches!(notice, X11Notice::NoServer { .. }), "{notice:?}");
            assert!(!notices.has_changed().unwrap(), "只说明一次");

            let (mut client, mut server) = tokio::io::duplex(1024);
            client
                .write_all(&setup(PROTOCOL.as_bytes(), &[0; 16]))
                .await
                .unwrap();
            assert_eq!(x11.forward(&mut server).await, Err("cookie 不对".into()));
        });
    }
}
