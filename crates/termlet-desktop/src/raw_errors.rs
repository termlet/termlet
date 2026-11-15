//! 把 Telnet、串口打不开的原因说成用户能理解的话（网络、跳板机类的沿用 SSH 的文案）。

use gpui_kit::SharedString;
use termlet_core::rawterm::RawError;
use termlet_core::serial::SerialError;

use crate::{ssh_errors, t};

pub fn describe(error: &RawError) -> SharedString {
    match error {
        RawError::Connect(error) => ssh_errors::describe(error),
        RawError::Serial(error) => describe_serial(error),
        RawError::NotRaw => t!("raw.error.not_raw"),
    }
}

pub fn describe_serial(error: &SerialError) -> SharedString {
    match error {
        SerialError::NotFound { port } => t!("raw.serial.not_found", port = port),
        SerialError::Busy { port } => t!("raw.serial.busy", port = port),
        SerialError::PermissionDenied { port } => t!("raw.serial.permission", port = port),
        SerialError::Unsupported { port, reason } => {
            t!("raw.serial.unsupported", port = port, reason = reason)
        }
        SerialError::Disconnected { port } => t!("raw.serial.disconnected", port = port),
        SerialError::Io { port, reason } => t!("raw.serial.io", port = port, reason = reason),
        SerialError::List(reason) => t!("raw.serial.list", reason = reason),
    }
}
