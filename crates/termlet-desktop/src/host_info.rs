//! 主机在列表中的简述：侧边栏名称下方的一行与悬停提示。SSH 不带前缀，Telnet、串口写出协议。Tab 的悬停提示中的连接
//! 目标也在这里（[`connection_target`]）。

use termlet_core::id::HostId;
use termlet_core::model::{Host, Parity, Protocol, SerialOptions, StopBits, join_host_port};
use termlet_core::store::Store;

use crate::t;

/// 主机按协议的图标（侧边栏的主机行、终端的 Tab）：SSH 为服务器，Telnet 为网络，串口为线缆；本地终端、WSL 是终端的图标。
pub fn protocol_icon(protocol: Protocol) -> gpui_kit::assets::IconName {
    use gpui_kit::assets::IconName;
    match protocol {
        Protocol::Ssh => IconName::Server,
        Protocol::Telnet => IconName::Network,
        Protocol::Serial => IconName::Cable,
    }
}

/// 名称下方的一行：`用户@地址`；Telnet 为"Telnet · 用户@地址"，串口为"串口 · COM3"。
pub fn summary(host: &Host) -> String {
    match host.protocol {
        Protocol::Ssh => user_at(host),
        Protocol::Telnet => t!("hosts.summary.telnet", target = user_at(host)).to_string(),
        Protocol::Serial => t!("hosts.summary.serial", port = host.address.clone()).to_string(),
    }
}

/// 悬停提示：`用户@地址:端口`；串口带上参数（"串口 · COM3 9600 7E1"）。
pub fn endpoint(host: &Host) -> String {
    let target = format!("{}:{}", user_at(host), host.effective_port());
    match host.protocol {
        Protocol::Ssh => target,
        Protocol::Telnet => t!("hosts.summary.telnet", target = target).to_string(),
        Protocol::Serial => t!(
            "hosts.endpoint.serial",
            port = host.address.clone(),
            params = serial_params(&host.serial)
        )
        .to_string(),
    }
}

/// 串口参数的简写：波特率 数据位 校验 停止位，例如 `115200 8N1`、`9600 7E1`。
pub fn serial_params(serial: &SerialOptions) -> String {
    let parity = match serial.parity {
        Parity::None => 'N',
        Parity::Odd => 'O',
        Parity::Even => 'E',
        Parity::Mark => 'M',
        Parity::Space => 'S',
    };
    let stop = match serial.stop_bits {
        StopBits::One => "1",
        StopBits::OnePointFive => "1.5",
        StopBits::Two => "2",
    };
    format!("{} {}{parity}{stop}", serial.baud, serial.data_bits)
}

fn user_at(host: &Host) -> String {
    if host.username.trim().is_empty() {
        host.address.clone()
    } else {
        format!("{}@{}", host.username, host.address)
    }
}

/// Tab 连的是什么（悬停提示中的连接目标，见 [`connection_target`]）。只有用户名、地址这些配置，不含密码等秘密。
pub enum ConnectionTarget<'a> {
    /// SSH（保存的主机、快速连接、被调起的）；`username` 为空时只写地址
    Ssh {
        username: &'a str,
        address: &'a str,
        port: u16,
    },
    Telnet {
        address: &'a str,
        port: u16,
    },
    /// 串口：端口名与波特率
    Serial {
        port: &'a str,
        baud: u32,
    },
    /// WSL 发行版；`user` 为空时是默认用户
    Wsl {
        distro: &'a str,
        user: &'a str,
    },
    /// 本地终端：没有连接目标
    Local,
}

/// Tab 的悬停提示中单独一行的连接目标：SSH 为 `用户@地址`，端口不是 22 时为 `用户@地址:端口`（IPv6 地址加方括号），
/// 没有用户名时只写地址；Telnet 为 `地址:端口`；串口为 `COM3 · 115200`；WSL 为发行版，指定了用户时为 `用户@发行版`；
/// 本地终端没有（`None`）。经跳板机的只写目标主机。
pub fn connection_target(target: &ConnectionTarget<'_>) -> Option<String> {
    Some(match *target {
        ConnectionTarget::Ssh {
            username,
            address,
            port,
        } => {
            let address = match port {
                22 => address.to_string(),
                port => join_host_port(address, port),
            };
            match username.trim() {
                "" => address,
                user => format!("{user}@{address}"),
            }
        }
        ConnectionTarget::Telnet { address, port } => join_host_port(address, port),
        ConnectionTarget::Serial { port, baud } => format!("{port} · {baud}"),
        ConnectionTarget::Wsl { distro, user } => match user.trim() {
            "" => distro.to_string(),
            user => format!("{user}@{distro}"),
        },
        ConnectionTarget::Local => return None,
    })
}

/// 保存的主机的连接目标（见 [`connection_target`]）：按主机现在的设置，用户名与连接时的解析相同（主机自己的、凭据档案的、
/// 分组的默认值），只是不取本机的用户名；主机已删除时为 `None`。
pub fn host_target(store: &Store, id: &HostId) -> Option<String> {
    let record = store.get::<Host>(id)?;
    let host = &record.doc;
    let port = host.effective_port();
    match host.protocol {
        Protocol::Ssh => {
            let username =
                termlet_core::import::effective_username(store, host, record.parent.as_deref());
            connection_target(&ConnectionTarget::Ssh {
                username: &username,
                address: &host.address,
                port,
            })
        }
        Protocol::Telnet => connection_target(&ConnectionTarget::Telnet {
            address: &host.address,
            port,
        }),
        Protocol::Serial => connection_target(&ConnectionTarget::Serial {
            port: &host.address,
            baud: host.serial.baud,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 连接目标：SSH 默认端口不写端口，别的端口写上（IPv6 这时加方括号），没有用户名只写地址；Telnet 总写端口；串口写
    // 波特率；WSL 指定了用户时带上用户；本地终端没有
    #[test]
    fn connection_targets_by_kind() {
        let ssh = |username, address, port| {
            connection_target(&ConnectionTarget::Ssh {
                username,
                address,
                port,
            })
        };
        assert_eq!(ssh("ops", "10.0.0.5", 22).as_deref(), Some("ops@10.0.0.5"));
        assert_eq!(
            ssh("ops", "web.example.com", 2222).as_deref(),
            Some("ops@web.example.com:2222")
        );
        assert_eq!(ssh("ops", "fe80::1", 22).as_deref(), Some("ops@fe80::1"));
        assert_eq!(
            ssh("ops", "2001:db8::5", 2222).as_deref(),
            Some("ops@[2001:db8::5]:2222")
        );
        assert_eq!(ssh("", "10.0.0.5", 22).as_deref(), Some("10.0.0.5"));
        assert_eq!(ssh(" ", "10.0.0.5", 2200).as_deref(), Some("10.0.0.5:2200"));
        assert_eq!(
            connection_target(&ConnectionTarget::Telnet {
                address: "192.168.1.1",
                port: 23
            })
            .as_deref(),
            Some("192.168.1.1:23")
        );
        assert_eq!(
            connection_target(&ConnectionTarget::Telnet {
                address: "::1",
                port: 2323
            })
            .as_deref(),
            Some("[::1]:2323")
        );
        assert_eq!(
            connection_target(&ConnectionTarget::Serial {
                port: "COM3",
                baud: 115_200
            })
            .as_deref(),
            Some("COM3 · 115200")
        );
        assert_eq!(
            connection_target(&ConnectionTarget::Wsl {
                distro: "Ubuntu",
                user: ""
            })
            .as_deref(),
            Some("Ubuntu")
        );
        assert_eq!(
            connection_target(&ConnectionTarget::Wsl {
                distro: "Ubuntu",
                user: "root"
            })
            .as_deref(),
            Some("root@Ubuntu")
        );
        assert_eq!(connection_target(&ConnectionTarget::Local), None);
    }

    // 保存的主机按协议写；用户名留空时沿用分组的默认用户名，都没有时只写地址
    #[test]
    fn host_targets_follow_the_saved_settings() {
        use termlet_core::model::{Group, GroupDefaults};
        use termlet_core::store::Position;
        let db = termlet_core::db::Database::open_in_memory().unwrap();
        let keys = Box::new(termlet_core::vault::MemoryKeyStore::default());
        let vault = termlet_core::vault::Vault::open(&db, keys).unwrap();
        let store = Store::open(db, vault).unwrap();
        let group = store
            .create(
                Group {
                    name: "prod".into(),
                    defaults: GroupDefaults {
                        username: "deploy".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                None,
                Position::Last,
            )
            .unwrap();
        let create = |host: Host, parent: Option<String>| {
            store.create(host, parent, Position::Last).unwrap()
        };
        let own = create(
            Host {
                address: "10.0.0.5".into(),
                port: 2222,
                username: "ops".into(),
                ..Default::default()
            },
            None,
        );
        let inherited = create(
            Host {
                address: "10.0.0.6".into(),
                ..Default::default()
            },
            Some(group.as_str().to_string()),
        );
        let bare = create(
            Host {
                address: "10.0.0.7".into(),
                ..Default::default()
            },
            None,
        );
        let telnet = create(
            Host {
                protocol: Protocol::Telnet,
                address: "10.0.0.8".into(),
                username: "admin".into(),
                ..Default::default()
            },
            None,
        );
        let serial = create(
            Host {
                protocol: Protocol::Serial,
                address: "COM3".into(),
                serial: SerialOptions {
                    baud: 9600,
                    ..Default::default()
                },
                ..Default::default()
            },
            None,
        );
        assert_eq!(
            host_target(&store, &own).as_deref(),
            Some("ops@10.0.0.5:2222")
        );
        assert_eq!(
            host_target(&store, &inherited).as_deref(),
            Some("deploy@10.0.0.6")
        );
        assert_eq!(host_target(&store, &bare).as_deref(), Some("10.0.0.7"));
        assert_eq!(host_target(&store, &telnet).as_deref(), Some("10.0.0.8:23"));
        assert_eq!(host_target(&store, &serial).as_deref(), Some("COM3 · 9600"));
        assert_eq!(
            host_target(&store, &HostId::from_raw("h_gone")),
            None,
            "主机已删除"
        );
    }

    #[test]
    fn serial_params_are_abbreviated() {
        let mut serial = SerialOptions::default();
        assert_eq!(serial_params(&serial), "115200 8N1");
        serial.baud = 9600;
        serial.data_bits = 7;
        serial.parity = Parity::Even;
        serial.stop_bits = StopBits::OnePointFive;
        assert_eq!(serial_params(&serial), "9600 7E1.5");
    }

    #[test]
    fn summaries_name_the_protocol() {
        crate::i18n::set_language(crate::i18n::Language::ZhCn);
        let mut host = Host {
            address: "10.0.0.1".into(),
            username: "admin".into(),
            ..Default::default()
        };
        assert_eq!(summary(&host), "admin@10.0.0.1");
        assert_eq!(endpoint(&host), "admin@10.0.0.1:22");
        host.protocol = Protocol::Telnet;
        assert_eq!(summary(&host), "Telnet · admin@10.0.0.1");
        assert_eq!(endpoint(&host), "Telnet · admin@10.0.0.1:23");
        host.protocol = Protocol::Serial;
        host.address = "COM3".into();
        host.username.clear();
        assert_eq!(summary(&host), "串口 · COM3");
        assert_eq!(endpoint(&host), "串口 · COM3 115200 8N1");
    }
}
