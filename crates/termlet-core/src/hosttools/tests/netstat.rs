//! 网络页的测试。

use std::collections::HashMap;

use super::{ShRunner, posix};
use crate::hosttools::netstat::{parse_conns, split_host_port};
use crate::hosttools::scripts::{MAX_CONNS, conn_script};
use crate::hosttools::{ConnList, ConnProcess, ConnSource, ToolsError};

// ss -tuanpe 的输出：Ubuntu 24.04（iproute2 6.1：0.0.0.0、[::]、*:80 双栈、%网卡、IPv4 映射地址、进程在前、
// uid 在 ino 前、root 的套接字没有 uid）。
const SS_UBUNTU: &str = r#"Netid State     Recv-Q Send-Q          Local Address:Port        Peer Address:Port Process
udp   UNCONN    0      0                  127.0.0.54:53               0.0.0.0:*     users:(("systemd-resolve",pid=612,fd=16)) uid:991 ino:7845 sk:1 cgroup:/system.slice/systemd-resolved.service <->
udp   UNCONN    0      0               127.0.0.53%lo:53               0.0.0.0:*     users:(("systemd-resolve",pid=612,fd=14)) uid:991 ino:7843 sk:2 cgroup:/system.slice/systemd-resolved.service <->
tcp   LISTEN    0      4096                  0.0.0.0:22               0.0.0.0:*     users:(("sshd",pid=1001,fd=3),("systemd",pid=1,fd=86)) ino:9123 sk:5 cgroup:/system.slice/ssh.socket <->
tcp   ESTAB     0      52                  10.0.2.15:22              10.0.2.2:51234 users:(("sshd",pid=2105,fd=4),("sshd",pid=2032,fd=4)) timer:(on,212ms,0) ino:20311 sk:6 cgroup:/user.slice/user-1000.slice/session-3.scope <->
tcp   LISTEN    0      511                         *:80                     *:*     users:(("nginx",pid=1400,fd=7),("nginx",pid=1399,fd=7)) ino:12001 sk:7 cgroup:/system.slice/nginx.service v6only:0 <->
tcp   LISTEN    0      4096                     [::]:22                  [::]:*     users:(("sshd",pid=1001,fd=4),("systemd",pid=1,fd=87)) ino:9125 sk:8 cgroup:/system.slice/ssh.socket v6only:1 <->
tcp   TIME-WAIT 0      0      [::ffff:10.0.2.15]:8080 [::ffff:10.0.2.2]:51300 timer:(timewait,45sec,0) ino:0 sk:9 <->
tcp   ESTAB     0      0                   10.0.2.15:41000         93.184.216.34:443   users:(("Web \"Content\"",pid=3000,fd=50)) uid:1000 ino:30001 sk:a <->
udp   UNCONN    0      0      [fe80::a00:27ff:fe4e:66a1]%enp0s3:546                  [::]:*     users:(("systemd-network",pid=580,fd=22)) uid:998 ino:7700 sk:3 cgroup:/system.slice/systemd-networkd.service v6only:1 <->
"#;

// CentOS 7（iproute 3.10）：任意地址写成 *，IPv6 任意地址写成 :::。
const SS_CENTOS7: &str = r#"Netid  State      Recv-Q Send-Q Local Address:Port               Peer Address:Port
udp    UNCONN     0      0         *:68                    *:*                   users:(("dhclient",pid=845,fd=6)) ino:17321 sk:ffff8800367e2000 <->
udp    UNCONN     0      0        ::1:323                  :::*                   users:(("chronyd",pid=650,fd=2)) uid:998 ino:15801 sk:ffff88003660d000 v6only:1 <->
tcp    LISTEN     0      128       *:22                    *:*                   users:(("sshd",pid=1090,fd=3)) ino:19870 sk:ffff880036b80000 <->
tcp    LISTEN     0      128      :::22                   :::*                   users:(("sshd",pid=1090,fd=4)) ino:19872 sk:ffff88003e6d0800 v6only:1 <->
tcp    ESTAB      0      0      192.168.1.10:22                 192.168.1.2:60718               users:(("sshd",pid=2210,fd=3)) timer:(keepalive,119min,0) ino:31000 sk:ffff880036b81f00 <->
tcp    ESTAB      0      0      fe80::1%eth0:22                 fe80::2%eth0:50000               users:(("sshd",2300,3)) ino:31001 sk:ffff880036b81f01 <->
"#;

fn process(pid: u32, name: &str) -> ConnProcess {
    ConnProcess {
        pid,
        name: name.into(),
    }
}

fn counts(list: &ConnList) -> HashMap<String, usize> {
    list.counts
        .iter()
        .map(|c| (format!("{} {}", c.proto, c.state), c.count))
        .collect()
}

#[test]
fn parse_ss_output() {
    let out = format!(
        "@@uid\n0\n@@passwd\nroot:0\nsystemd-resolve:991\ntermlet:1000\n@@counts\ntcp LISTEN 3\ntcp ESTAB 2\n\
         tcp TIME-WAIT 1\nudp UNCONN 3\n@@ss\n{SS_UBUNTU}"
    );
    let list = parse_conns(&out).unwrap();
    assert_eq!(list.source, ConnSource::Ss);
    assert_eq!(
        (list.conns.len(), list.total, list.truncated),
        (9, 9, false)
    );
    assert_eq!(list.uid, Some(0));
    let c = &list.conns;
    assert_eq!(
        (
            c[1].local_addr.as_str(),
            c[1].local_port,
            c[1].proto.as_str(),
            c[1].state.as_str()
        ),
        ("127.0.0.53", 53, "udp", "UNCONN"),
        "UDP 与网卡后缀"
    );
    assert_eq!(c[1].uid, Some(991));
    assert_eq!(c[1].user.as_deref(), Some("systemd-resolve"));
    assert_eq!((c[1].peer_addr.as_str(), c[1].peer_port), ("0.0.0.0", 0));
    assert_eq!(c[2].uid, Some(0), "root 的套接字");
    assert_eq!(c[2].user.as_deref(), Some("root"));
    assert_eq!(c[2].processes.len(), 2, "多个进程");
    assert_eq!(c[2].processes[0], process(1001, "sshd"));
    assert_eq!(
        (c[3].send_q, c[3].peer_addr.as_str(), c[3].peer_port),
        (52, "10.0.2.2", 51234)
    );
    assert_eq!(
        (
            c[4].local_addr.as_str(),
            c[4].local_port,
            c[4].processes.len()
        ),
        ("*", 80, 2),
        "双栈任意地址"
    );
    assert_eq!(
        (c[5].local_addr.as_str(), c[5].peer_addr.as_str()),
        ("::", "::")
    );
    assert_eq!(c[6].local_addr, "::ffff:10.0.2.15", "IPv4 映射地址");
    assert_eq!(c[6].state, "TIME-WAIT");
    assert!(c[6].processes.is_empty());
    assert_eq!(
        c[7].processes,
        [process(3000, r#"Web "Content""#)],
        "名称中的引号"
    );
    assert_eq!(c[7].user.as_deref(), Some("termlet"));
    // IPv6 链路本地地址：ss 把网卡后缀写在方括号之后
    assert_eq!(
        (
            c[8].local_addr.as_str(),
            c[8].local_port,
            c[8].peer_addr.as_str(),
            c[8].uid
        ),
        ("fe80::a00:27ff:fe4e:66a1", 546, "::", Some(998))
    );
}

/// ss 不转义进程名，名称中的换行把一行拆成几行，后面的行以 @@ 开头也伪造不了段
/// （@@unsupported 让整页显示"只支持 Linux"、@@hide 把之后的连接都藏起来）；名称中写的 uid: 也不算。
#[test]
fn ss_forged_markers() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 1000");
    runner.fake(
        "ss",
        r#"[ "$1" = -tuanpe ] || { printf 'Netid State\ntcp LISTEN\ntcp LISTEN\ntcp LISTEN\n'; exit 0; }
cat <<'EOF'
Netid State  Recv-Q Send-Q Local Address:Port Peer Address:Port Process
tcp   LISTEN 0      128          0.0.0.0:8080      0.0.0.0:*    users:(("x
@@unsupported",pid=4242,fd=3)) ino:1 sk:1 <->
tcp   LISTEN 0      128          0.0.0.0:9090      0.0.0.0:*    users:(("a uid:0
@@hide",pid=4343,fd=3)) uid:1000 ino:2 sk:2 <->
tcp   LISTEN 0      128          0.0.0.0:22        0.0.0.0:*    users:(("sshd",pid=1001,fd=3)) ino:3 sk:3 <->
EOF"#,
    );
    let list = parse_conns(&runner.run(&conn_script(), &[], &[]))
        .expect("伪造的 @@unsupported 不应起作用");
    assert_eq!(
        (list.conns.len(), list.total, list.truncated),
        (3, 3, false),
        "伪造的段不应藏起后面的连接"
    );
    let c = &list.conns;
    assert_eq!(
        c[0].processes,
        [process(4242, "x\n@@unsupported")],
        "名称中有换行"
    );
    assert_eq!(c[0].uid, Some(0));
    assert_eq!(c[1].processes.len(), 1);
    assert_eq!(c[1].processes[0].name, "a uid:0\n@@hide");
    assert_eq!(c[1].uid, Some(1000), "名称中的 uid: 不算");
    assert_eq!(
        (c[2].local_port, c[2].processes[0].name.as_str()),
        (22, "sshd")
    );
}

/// CentOS 7 上旧版本 ss 的任意地址、IPv6、链路本地地址与进程的写法。
#[test]
fn parse_ss_centos7() {
    let list = parse_conns(&format!("@@uid\n1000\n@@counts\n@@ss\n{SS_CENTOS7}")).unwrap();
    let c = &list.conns;
    assert_eq!((c.len(), list.total), (6, 6));
    assert_eq!(
        (
            c[0].local_addr.as_str(),
            c[0].local_port,
            c[0].peer_addr.as_str()
        ),
        ("*", 68, "*")
    );
    assert_eq!(
        (c[1].local_addr.as_str(), c[1].peer_addr.as_str(), c[1].uid),
        ("::1", "::", Some(998))
    );
    assert_eq!(
        (
            c[3].local_addr.as_str(),
            c[3].local_port,
            c[3].state.as_str()
        ),
        ("::", 22, "LISTEN")
    );
    assert_eq!(c[5].local_addr, "fe80::1");
    assert_eq!(c[5].processes[0], process(2300, "sshd"));
}

// 没有 ss 的老系统、Alpine（BusyBox 没有 ss）：读 /proc/net。
const PROC_TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 18093 1 0000000000000000 100 0 0 10 0
   1: 020011AC:0016 010011AC:D432 01 00000034:00000000 02:000A8C1E 00000000  1000        0 51237 2 0000000000000000 20 4 30 10 -1
";
const PROC_TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 18095 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000100007F:1F90 0000000000000000FFFF00000100007F:C350 06 00000000:00000000 00:00000000 00000000  1000        0 52000 1 0000000000000000 20 4 30 10 -1
";
const PROC_UDP: &str = "   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops
  123: 0B00007F:8C1B 00000000:0000 07 00000000:00000000 00:00000000 00000000     0        0 17800 2 0000000000000000 0
";

/// 小端地址、IPv6 与映射地址、UDP 的 UNCONN、各状态的总数（tcp 与 tcp6 合并）。
#[test]
fn conn_script_without_ss() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in [
        ("tcp", PROC_TCP),
        ("tcp6", PROC_TCP6),
        ("udp", PROC_UDP),
        ("udp6", ""),
    ] {
        let path = dir.path().join("proc/net").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    std::fs::write(
        dir.path().join("passwd"),
        "root:x:0:0::/root:/bin/sh\ntermlet:x:1000:1000::/home/termlet:/bin/bash\n",
    )
    .unwrap();
    runner.fake("id", "echo 1000");
    let net = format!("{}/proc/net", posix(dir.path()));
    let passwd = format!("{}/passwd", posix(dir.path()));
    let out = runner.run(
        &conn_script(),
        &[
            ("/proc/net", net.as_str()),
            ("/etc/passwd", passwd.as_str()),
        ],
        &[],
    );
    let list = parse_conns(&out).unwrap_or_else(|e| panic!("{e}\n{out}"));
    assert_eq!(list.source, ConnSource::Proc, "{out}");
    assert_eq!((list.conns.len(), list.total, list.uid), (5, 5, Some(1000)));
    let c = &list.conns;
    assert_eq!(
        (
            c[0].local_addr.as_str(),
            c[0].local_port,
            c[0].state.as_str(),
            c[0].user.as_deref()
        ),
        ("0.0.0.0", 22, "LISTEN", Some("root"))
    );
    assert_eq!(
        (
            c[1].local_addr.as_str(),
            c[1].peer_addr.as_str(),
            c[1].peer_port,
            c[1].state.as_str(),
            c[1].send_q,
            c[1].user.as_deref()
        ),
        (
            "172.17.0.2",
            "172.17.0.1",
            54322,
            "ESTAB",
            0x34,
            Some("termlet")
        ),
        "已连接（小端地址）"
    );
    assert_eq!(c[2].local_addr, "::");
    assert_eq!(
        (
            c[3].local_addr.as_str(),
            c[3].local_port,
            c[3].state.as_str()
        ),
        ("::ffff:127.0.0.1", 8080, "TIME-WAIT")
    );
    assert_eq!(
        (
            c[4].proto.as_str(),
            c[4].state.as_str(),
            c[4].local_addr.as_str(),
            c[4].local_port
        ),
        ("udp", "UNCONN", "127.0.0.11", 35867)
    );
    let counts = counts(&list);
    assert_eq!(counts.get("tcp LISTEN"), Some(&2), "{counts:?}");
    assert_eq!(counts.get("tcp ESTAB"), Some(&1));
    assert_eq!(counts.get("udp UNCONN"), Some(&1));
}

#[test]
fn conn_script_with_ss() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    runner.fake("id", "echo 0");
    runner.fake("ss", &format!("cat <<'EOF'\n{SS_UBUNTU}EOF"));
    let list = parse_conns(&runner.run(&conn_script(), &[], &[])).unwrap();
    let counts = counts(&list);
    assert_eq!(list.source, ConnSource::Ss);
    assert_eq!(list.conns.len(), 9);
    assert_eq!(counts.get("tcp LISTEN"), Some(&3), "{counts:?}");
    assert_eq!(counts.get("udp UNCONN"), Some(&3));
    assert_eq!(list.total, 9);
    assert!(!list.sudo);
}

/// 有免密码 sudo 时（`$1` 为 sudo）经 `sudo -n ss` 读，看得到别的用户的进程名；sudo 失败时退回不带 sudo 的读法；
/// 没有要求时不调用 sudo。总数不经 sudo。
#[test]
fn conn_script_reads_with_sudo() {
    let Some(runner) = ShRunner::isolated() else {
        return;
    };
    let marker = tempfile::tempdir().unwrap();
    let called = posix(&marker.path().join("called"));
    runner.fake("id", "echo 1000");
    // 普通用户读到的没有进程，经 sudo 读到的有
    runner.fake(
        "ss",
        r#"if [ "$SUDO_RUN" = 1 ]; then p=' users:(("nginx",pid=42,fd=6)) ino:1 sk:1 <->'; else p=' ino:1 sk:1 <->'; fi
printf 'Netid State Recv-Q Send-Q Local Address:Port Peer Address:Port Process\n'
printf 'tcp LISTEN 0 511 0.0.0.0:80 0.0.0.0:*%s\n' "$p""#,
    );
    runner.fake(
        "sudo",
        &format!(r#"touch '{called}'; [ "$1" = -n ] && shift; SUDO_RUN=1 exec "$@""#),
    );
    let list = parse_conns(&runner.run(&conn_script(), &[], &["sudo"])).unwrap();
    assert!(list.sudo);
    assert_eq!(list.conns[0].processes, [process(42, "nginx")]);
    assert_eq!(list.total, 1, "总数照常读");

    // 没有要求：不调用 sudo
    std::fs::remove_file(marker.path().join("called")).unwrap();
    let list = parse_conns(&runner.run(&conn_script(), &[], &[""])).unwrap();
    assert!(!list.sudo && list.conns[0].processes.is_empty());
    assert!(
        !marker.path().join("called").exists(),
        "没有要求时不调用 sudo"
    );

    // sudo 失败（例如 sudoers 只允许别的命令）：退回不带 sudo 的读法
    runner.fake("sudo", r#"echo "sudo: a password is required" >&2; exit 1"#);
    let list = parse_conns(&runner.run(&conn_script(), &[], &["sudo"])).unwrap();
    assert!(!list.sudo);
    assert_eq!(list.conns.len(), 1);
}

/// 只列出前 3000 个，总数取各状态的计数；不是 Linux。
#[test]
fn conns_truncated() {
    let mut out = String::from("@@uid\n0\n@@counts\ntcp ESTAB 5000\n@@ss\nNetid State\n");
    for _ in 0..=MAX_CONNS {
        out.push_str("tcp ESTAB 0 0 10.0.0.1:22 10.0.0.2:40000\n");
    }
    let list = parse_conns(&out).unwrap();
    assert_eq!(
        (list.conns.len(), list.total, list.truncated),
        (MAX_CONNS, 5000, true)
    );
    assert_eq!(
        parse_conns("@@unsupported\nDarwin\n"),
        Err(ToolsError::Unsupported)
    );
}

#[test]
fn host_port() {
    for (input, host, port) in [
        ("0.0.0.0:22", "0.0.0.0", 22),
        ("[::]:80", "::", 80),
        ("*:*", "*", 0),
        (":::22", "::", 22),
        ("*:22", "*", 22),
        ("127.0.0.53%lo:53", "127.0.0.53", 53),
        ("[fe80::1]%eth0:546", "fe80::1", 546),
        ("fe80::1%eth0:22", "fe80::1", 22),
        ("[::ffff:1.2.3.4]:443", "::ffff:1.2.3.4", 443),
    ] {
        assert_eq!(
            split_host_port(input),
            Some((host.to_string(), port)),
            "{input}"
        );
    }
    for bad in ["nocolon", "1.2.3.4:x", "1.2.3.4:70000"] {
        assert_eq!(split_host_port(bad), None, "应拒绝 {bad}");
    }
}
