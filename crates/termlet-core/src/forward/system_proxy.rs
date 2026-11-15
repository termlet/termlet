//! 本机的系统代理（“通过本机代理”没有在设置中填写本机代理时用）：
//! Windows 读“Internet 选项”的代理设置（注册表），macOS 读 `scutil --proxy`，之后（以及 Linux）看环境变量
//! `https_proxy`、`http_proxy`、`all_proxy`（大小写均可）。只认 HTTP 代理：SOCKS 代理与自动配置脚本（PAC）不认。

use super::share::ProxyAddress;

/// 从哪里得到的本机代理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxySource {
    /// 设置中填写的
    Settings,
    /// 系统的代理设置
    System,
    /// 环境变量
    Environment,
}

/// 要用的本机代理：设置中填了的用它（写得不对时为 `None`），没填时找系统代理。
pub fn local_proxy(setting: &str) -> Option<(ProxyAddress, ProxySource)> {
    if !setting.trim().is_empty() {
        return ProxyAddress::parse(setting).map(|a| (a, ProxySource::Settings));
    }
    system_proxy()
}

/// 系统代理（见模块说明）；没有时为 `None`。
pub fn system_proxy() -> Option<(ProxyAddress, ProxySource)> {
    os_proxy().map(|a| (a, ProxySource::System)).or_else(|| {
        env_proxy(|name| std::env::var(name).ok()).map(|a| (a, ProxySource::Environment))
    })
}

/// 环境变量中的代理；`get` 读一个变量。
fn env_proxy(get: impl Fn(&str) -> Option<String>) -> Option<ProxyAddress> {
    [
        "https_proxy",
        "HTTPS_PROXY",
        "http_proxy",
        "HTTP_PROXY",
        "all_proxy",
        "ALL_PROXY",
    ]
    .iter()
    .filter_map(|name| get(name))
    .find_map(|value| ProxyAddress::parse(&value))
}

/// Windows 的 `ProxyServer` 值：`主机:端口`，或者按协议分开写的 `http=主机:端口;https=主机:端口;socks=…`。
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_server(value: &str) -> Option<ProxyAddress> {
    if !value.contains('=') {
        return ProxyAddress::parse(value);
    }
    let entries: Vec<(&str, &str)> = value
        .split(';')
        .filter_map(|part| part.split_once('='))
        .map(|(k, v)| (k.trim(), v.trim()))
        .collect();
    ["https", "http"].iter().find_map(|wanted| {
        entries
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(wanted))
            .and_then(|(_, v)| ProxyAddress::parse(v))
    })
}

/// `scutil --proxy` 的输出中的 HTTPS 或 HTTP 代理。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_scutil(output: &str) -> Option<ProxyAddress> {
    let value = |key: &str| {
        output.lines().find_map(|line| {
            let (k, v) = line.split_once(" : ")?;
            (k.trim() == key).then(|| v.trim().to_string())
        })
    };
    ["HTTPS", "HTTP"].iter().find_map(|prefix| {
        if value(&format!("{prefix}Enable")).as_deref() != Some("1") {
            return None;
        }
        let host = value(&format!("{prefix}Proxy"))?;
        let port: u16 = value(&format!("{prefix}Port"))?.parse().ok()?;
        ProxyAddress::parse(&crate::model::join_host_port(&host, port))
    })
}

#[cfg(windows)]
fn os_proxy() -> Option<ProxyAddress> {
    let key = windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings")
        .ok()?;
    if key.get_u32("ProxyEnable").ok()? == 0 {
        return None;
    }
    parse_windows_server(&key.get_string("ProxyServer").ok()?)
}

#[cfg(target_os = "macos")]
fn os_proxy() -> Option<ProxyAddress> {
    let output = std::process::Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    parse_scutil(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(any(windows, target_os = "macos")))]
fn os_proxy() -> Option<ProxyAddress> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(a: Option<ProxyAddress>) -> Option<(String, u16)> {
        a.map(|a| (a.host, a.port))
    }

    #[test]
    fn windows_proxy_server_values() {
        assert_eq!(
            pair(parse_windows_server("127.0.0.1:7890")),
            Some(("127.0.0.1".into(), 7890))
        );
        assert_eq!(
            pair(parse_windows_server(
                "http=127.0.0.1:8080;https=127.0.0.1:8443;socks=127.0.0.1:1080"
            )),
            Some(("127.0.0.1".into(), 8443))
        );
        assert_eq!(pair(parse_windows_server("socks=127.0.0.1:1080")), None);
        assert_eq!(pair(parse_windows_server("")), None);
    }

    #[test]
    fn scutil_output() {
        let out = "<dictionary> {\n  HTTPEnable : 1\n  HTTPPort : 7890\n  HTTPProxy : 127.0.0.1\n  HTTPSEnable : 0\n  SOCKSEnable : 1\n}\n";
        assert_eq!(pair(parse_scutil(out)), Some(("127.0.0.1".into(), 7890)));
        assert_eq!(
            pair(parse_scutil("<dictionary> {\n  HTTPEnable : 0\n}")),
            None
        );
    }

    #[test]
    fn environment_variables() {
        let vars = |list: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                list.iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            pair(env_proxy(vars(&[
                ("all_proxy", "socks5://127.0.0.1:1080"),
                ("HTTP_PROXY", "http://127.0.0.1:3128")
            ]))),
            Some(("127.0.0.1".into(), 3128))
        );
        assert_eq!(
            pair(env_proxy(vars(&[("ALL_PROXY", "socks5://h:1")]))),
            None
        );
        assert_eq!(
            local_proxy(" http://10.0.0.2:8080 ").map(|(a, s)| (a.port, s)),
            Some((8080, ProxySource::Settings))
        );
        assert_eq!(local_proxy("socks5://x:1"), None);
    }
}
