//! 通过本机代理时给出的命令片段：服务器上的程序怎样用 `http://127.0.0.1:端口` 这个代理，以及怎样撤销。
//!
//! 都是在服务器上执行的 POSIX shell 命令，写入系统配置的要 sudo；写入的文件名带 `termlet`，撤销时只删它们。
//! 界面按 [`SnippetKind`] 配上说明文字。

/// 片段的用途。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SnippetKind {
    /// 当前 shell 的环境变量（curl、wget、git、pip 等认它们）
    Shell,
    /// apt（Debian、Ubuntu）：`/etc/apt/apt.conf.d/` 下的配置文件
    Apt,
    /// dnf（Fedora、RHEL 8 起）：`/etc/dnf/dnf.conf` 中加一行
    Dnf,
    /// yum（CentOS 7 等）：`/etc/yum.conf` 中加一行
    Yum,
    /// Docker 守护进程（拉取镜像）：systemd 的 drop-in，要重启 Docker
    DockerDaemon,
    /// `docker build`：构建时的环境变量，用主机网络才连得到回环地址；只对这一次有效，不用撤销
    DockerBuild,
}

impl SnippetKind {
    pub const ALL: [SnippetKind; 6] = [
        SnippetKind::Shell,
        SnippetKind::Apt,
        SnippetKind::Dnf,
        SnippetKind::Yum,
        SnippetKind::DockerDaemon,
        SnippetKind::DockerBuild,
    ];
}

/// 一个命令片段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxySnippet {
    pub kind: SnippetKind,
    /// 开始使用代理的命令
    pub apply: String,
    /// 撤销的命令；只对一条命令有效的（`docker build`）没有
    pub undo: Option<String>,
}

/// 不经代理的地址（服务器本机）。
const NO_PROXY: &str = "localhost,127.0.0.1";
/// apt 的配置文件。
const APT_FILE: &str = "/etc/apt/apt.conf.d/95termlet-proxy";
/// Docker 的 systemd drop-in。
const DOCKER_DIR: &str = "/etc/systemd/system/docker.service.d";
const DOCKER_FILE: &str = "/etc/systemd/system/docker.service.d/termlet-proxy.conf";

/// 服务器上用的代理地址。
pub fn proxy_url(port: u16) -> String {
    format!("http://{}:{port}", super::share::SHARE_BIND)
}

/// 端口为 `port` 时的全部片段，按 [`SnippetKind::ALL`] 的顺序。
pub fn proxy_snippets(port: u16) -> Vec<ProxySnippet> {
    SnippetKind::ALL
        .iter()
        .map(|kind| proxy_snippet(*kind, port))
        .collect()
}

/// 一种用途的片段。
pub fn proxy_snippet(kind: SnippetKind, port: u16) -> ProxySnippet {
    let url = proxy_url(port);
    // sed 的地址用 `\#…#` 定界，免得转义 URL 中的斜杠；点转义成字面的点
    let line = format!("proxy={url}");
    let line_pattern = line.replace('.', r"\.");
    let restart = "sudo systemctl daemon-reload && sudo systemctl restart docker";
    let (apply, undo) = match kind {
        SnippetKind::Shell => (
            format!("export http_proxy={url} https_proxy={url} no_proxy={NO_PROXY}"),
            Some("unset http_proxy https_proxy no_proxy".to_string()),
        ),
        SnippetKind::Apt => (
            format!(
                r#"echo 'Acquire::http::Proxy "{url}/"; Acquire::https::Proxy "{url}/";' | sudo tee {APT_FILE} >/dev/null"#
            ),
            Some(format!("sudo rm -f {APT_FILE}")),
        ),
        SnippetKind::Dnf | SnippetKind::Yum => {
            let file = if kind == SnippetKind::Dnf {
                "/etc/dnf/dnf.conf"
            } else {
                "/etc/yum.conf"
            };
            (
                format!("echo '{line}' | sudo tee -a {file} >/dev/null"),
                Some(format!(r"sudo sed -i '\#^{line_pattern}$#d' {file}")),
            )
        }
        SnippetKind::DockerDaemon => (
            format!(
                r#"sudo mkdir -p {DOCKER_DIR} && printf '[Service]\nEnvironment="HTTP_PROXY={url}" "HTTPS_PROXY={url}" "NO_PROXY={NO_PROXY}"\n' | sudo tee {DOCKER_FILE} >/dev/null && {restart}"#
            ),
            Some(format!("sudo rm -f {DOCKER_FILE} && {restart}")),
        ),
        SnippetKind::DockerBuild => (
            format!(
                "docker build --network host --build-arg http_proxy={url} --build-arg https_proxy={url} ."
            ),
            None,
        ),
    };
    ProxySnippet { kind, apply, undo }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_use_the_port_and_undo_what_they_write() {
        let all = proxy_snippets(38491);
        assert_eq!(all.len(), SnippetKind::ALL.len());
        let get = |kind| all.iter().find(|s| s.kind == kind).unwrap();
        let shell = get(SnippetKind::Shell);
        assert_eq!(
            shell.apply,
            "export http_proxy=http://127.0.0.1:38491 https_proxy=http://127.0.0.1:38491 no_proxy=localhost,127.0.0.1"
        );
        assert_eq!(
            shell.undo.as_deref(),
            Some("unset http_proxy https_proxy no_proxy")
        );
        let apt = get(SnippetKind::Apt);
        assert!(
            apt.apply
                .contains(r#"Acquire::http::Proxy "http://127.0.0.1:38491/";"#)
        );
        assert!(
            apt.apply
                .contains("sudo tee /etc/apt/apt.conf.d/95termlet-proxy")
        );
        assert_eq!(
            apt.undo.as_deref(),
            Some("sudo rm -f /etc/apt/apt.conf.d/95termlet-proxy")
        );
        let dnf = get(SnippetKind::Dnf);
        assert_eq!(
            dnf.apply,
            "echo 'proxy=http://127.0.0.1:38491' | sudo tee -a /etc/dnf/dnf.conf >/dev/null"
        );
        assert_eq!(
            dnf.undo.as_deref(),
            Some(r"sudo sed -i '\#^proxy=http://127\.0\.0\.1:38491$#d' /etc/dnf/dnf.conf")
        );
        assert!(
            get(SnippetKind::Yum)
                .apply
                .ends_with("/etc/yum.conf >/dev/null")
        );
        let docker = get(SnippetKind::DockerDaemon);
        assert!(
            docker
                .apply
                .contains(r#"Environment="HTTP_PROXY=http://127.0.0.1:38491""#)
        );
        assert!(docker.apply.ends_with("sudo systemctl restart docker"));
        assert!(
            docker.undo.as_deref().unwrap().starts_with(
                "sudo rm -f /etc/systemd/system/docker.service.d/termlet-proxy.conf &&"
            )
        );
        let build = get(SnippetKind::DockerBuild);
        assert!(build.apply.contains("--network host"));
        assert!(
            build
                .apply
                .contains("--build-arg https_proxy=http://127.0.0.1:38491")
        );
        assert_eq!(build.undo, None);
    }

    /// 用本机的 sh 执行：export 设上的变量都是这个地址，unset 后都没有了；dnf 的撤销只删加上的那一行。
    #[test]
    fn shell_snippets_run_in_sh() {
        let Some(sh) = which_sh() else {
            return;
        };
        let shell = proxy_snippet(SnippetKind::Shell, 3128);
        let script = format!(
            "{}; printf '%s|%s|%s\\n' \"$http_proxy\" \"$https_proxy\" \"$no_proxy\"; {}; printf '[%s%s%s]' \"$http_proxy\" \"$https_proxy\" \"$no_proxy\"",
            shell.apply,
            shell.undo.unwrap()
        );
        let out = std::process::Command::new(&sh)
            .args(["-c", &script])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "http://127.0.0.1:3128|http://127.0.0.1:3128|localhost,127.0.0.1\n[]"
        );

        // dnf：把命令中的 sudo 与文件换成临时文件再执行
        let dir = std::env::temp_dir().join(format!(
            "termlet-proxy-snippet-{}",
            crate::id::random_token(6)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("dnf.conf");
        std::fs::write(&conf, "[main]\ngpgcheck=1\nproxy=http://127.0.0.1:31280\n").unwrap();
        let path = conf.to_string_lossy().replace('\\', "/");
        let dnf = proxy_snippet(SnippetKind::Dnf, 3128);
        let localize = |cmd: &str| cmd.replace("sudo ", "").replace("/etc/dnf/dnf.conf", &path);
        for cmd in [localize(&dnf.apply), localize(dnf.undo.as_deref().unwrap())] {
            let status = std::process::Command::new(&sh)
                .args(["-c", &cmd])
                .status()
                .unwrap();
            assert!(status.success(), "{cmd}");
        }
        let after = std::fs::read_to_string(&conf).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(after, "[main]\ngpgcheck=1\nproxy=http://127.0.0.1:31280\n");
    }

    /// 本机的 sh（Windows 上是 Git 的 sh）；没有时跳过。
    fn which_sh() -> Option<std::path::PathBuf> {
        let candidates: &[&str] = if cfg!(windows) {
            &[
                r"C:\Program Files\Git\usr\bin\sh.exe",
                r"C:\Program Files\Git\bin\sh.exe",
            ]
        } else {
            &["/bin/sh"]
        };
        candidates
            .iter()
            .map(std::path::PathBuf::from)
            .find(|p| p.exists())
    }
}
