//! 把保存条目时的错误（字段校验、引用检查、写库失败）说成用户能理解的话。

use gpui_kit::SharedString;
use termlet_core::{CoreError, Invalid};

use crate::t;

pub fn describe(error: &CoreError) -> SharedString {
    match error {
        CoreError::Invalid(invalid) => describe_invalid(invalid),
        CoreError::VaultLocked => t!("store.error.vault_locked"),
        CoreError::NotFound(_) => t!("store.error.not_found"),
        other => t!("store.error.failed", error = other.to_string()),
    }
}

pub fn describe_invalid(invalid: &Invalid) -> SharedString {
    match (invalid.field, invalid.reason) {
        ("name", "required") => t!("store.invalid.name_required"),
        ("name", "too_long") => t!("store.invalid.name_too_long"),
        ("address", "invalid") => t!("store.invalid.address"),
        ("address", "too_long") => t!("store.invalid.address_too_long"),
        ("address", "invalid_serial_port") => t!("store.invalid.serial_port"),
        ("telnet.termType", _) => t!("store.invalid.term_type"),
        ("serial.baud", _) => t!("store.invalid.serial_baud"),
        ("serial.dataBits", _) => t!("store.invalid.serial_data_bits"),
        ("serial.usb", _) => t!("store.invalid.serial_usb"),
        ("username" | "defaults.username", "too_long") => t!("store.invalid.username_too_long"),
        ("note", "too_long") => t!("store.invalid.note_too_long"),
        ("tags", "too_long") => t!("store.invalid.tag_too_long"),
        ("tags", "too_many") => t!("store.invalid.too_many_tags"),
        ("jump", "cycle") => t!("store.invalid.jump_cycle"),
        ("jump", "too_deep") => t!("store.invalid.jump_too_deep"),
        ("jump" | "defaults.jump", "not_found") => t!("store.invalid.jump_not_found"),
        ("host", "not_ssh") => t!("store.invalid.jump_not_ssh"),
        ("auth.profile" | "defaults.profile", "not_found") => t!("store.invalid.profile_not_found"),
        ("profile", "in_use") => t!("store.invalid.profile_in_use"),
        ("key", "in_use") => t!("store.invalid.key_in_use"),
        ("parent", "cycle") => t!("store.invalid.parent_cycle"),
        ("parent", "not_found") => t!("store.invalid.parent_not_found"),
        ("proxy.address", _) => t!("store.invalid.proxy_address"),
        ("proxy.port", _) => t!("store.invalid.proxy_port"),
        ("proxy.command", "required") => t!("store.invalid.proxy_command_required"),
        ("proxy.command" | "defaults.proxyCommand", _) => t!("store.invalid.proxy_command"),
        ("terminal.startup", "too_many_lines") => t!("store.invalid.startup_too_many_lines"),
        ("terminal.startup", "line_too_long") => t!("store.invalid.startup_line_too_long"),
        ("terminal.startup", _) => t!("store.invalid.startup_control_chars"),
        (field, reason) => t!("store.invalid.other", field = field, reason = reason),
    }
}
