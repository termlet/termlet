//! 主机在列表中的简述：侧边栏名称下方的一行与悬停提示。SSH 不带前缀，Telnet、串口写出协议。

use termlet_core::model::{Host, Parity, Protocol, SerialOptions, StopBits};

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

#[cfg(test)]
mod tests {
    use super::*;

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
