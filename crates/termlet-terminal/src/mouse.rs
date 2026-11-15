//! 鼠标上报：远端程序开启鼠标模式（vim、tmux、htop 等）时，把点击、拖动、滚轮编码成转义序列。

use crate::grid::{MouseEncoding, MouseTracking, TerminalModes};

/// 鼠标按键。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    /// 只移动、没有按下任何键（MOUSE_MOTION 模式）
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

/// 远端是否要求上报这个事件。
pub fn wants(mode: TerminalModes, action: MouseAction, button: MouseButton) -> bool {
    match (action, mode.mouse_tracking) {
        (_, MouseTracking::Off) => false,
        (MouseAction::Motion, tracking) if button == MouseButton::None => {
            tracking == MouseTracking::Motion
        }
        (MouseAction::Motion, tracking) => {
            matches!(tracking, MouseTracking::Motion | MouseTracking::Drag)
        }
        _ => true,
    }
}

/// 编码一次鼠标事件。`column`、`line` 是从 0 开始的格子坐标（可见区内）。
pub fn encode(
    mode: TerminalModes,
    action: MouseAction,
    button: MouseButton,
    modifiers: MouseModifiers,
    column: usize,
    line: usize,
) -> Option<Vec<u8>> {
    if !wants(mode, action, button) {
        return None;
    }
    let mut code: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::None => 3,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if action == MouseAction::Motion {
        code += 32;
    }
    if modifiers.shift {
        code += 4;
    }
    if modifiers.alt {
        code += 8;
    }
    if modifiers.control {
        code += 16;
    }
    let x = column as u32 + 1;
    let y = line as u32 + 1;

    if mode.mouse_encoding == MouseEncoding::Sgr {
        let suffix = if action == MouseAction::Release {
            'm'
        } else {
            'M'
        };
        return Some(format!("\x1b[<{code};{x};{y}{suffix}").into_bytes());
    }

    // 普通格式：松开时按键号为 3（不区分哪个键），坐标加 32 后作为一个字节（UTF-8 模式下按 UTF-8 编码）
    if action == MouseAction::Release {
        code = 3 + (code & !3 & !64);
    }
    let mut bytes = b"\x1b[M".to_vec();
    bytes.push((32 + code) as u8);
    let utf8 = mode.mouse_encoding == MouseEncoding::Utf8;
    for value in [x, y] {
        let encoded = 32 + value;
        if utf8 {
            let ch = char::from_u32(encoded.min(2047))?;
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        } else {
            // 超出 223 的坐标无法用一个字节表示，不上报
            if encoded > 255 {
                return None;
            }
            bytes.push(encoded as u8);
        }
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLICK: TerminalModes = modes(MouseTracking::Click, MouseEncoding::Default);

    const fn modes(tracking: MouseTracking, encoding: MouseEncoding) -> TerminalModes {
        TerminalModes {
            alt_screen: false,
            show_cursor: true,
            app_cursor: false,
            bracketed_paste: false,
            line_feed_new_line: false,
            alternate_scroll: false,
            mouse_tracking: tracking,
            mouse_encoding: encoding,
            keyboard: crate::KeyboardFlags::NONE,
        }
    }

    #[test]
    fn nothing_is_reported_without_mouse_mode() {
        assert_eq!(
            encode(
                TerminalModes::default(),
                MouseAction::Press,
                MouseButton::Left,
                Default::default(),
                0,
                0
            ),
            None
        );
    }

    #[test]
    fn sgr_press_and_release() {
        let mode = modes(MouseTracking::Click, MouseEncoding::Sgr);
        let press = encode(
            mode,
            MouseAction::Press,
            MouseButton::Left,
            Default::default(),
            4,
            9,
        )
        .unwrap();
        assert_eq!(press, b"\x1b[<0;5;10M");
        let release = encode(
            mode,
            MouseAction::Release,
            MouseButton::Left,
            Default::default(),
            4,
            9,
        )
        .unwrap();
        assert_eq!(release, b"\x1b[<0;5;10m");
        let wheel = encode(
            mode,
            MouseAction::Press,
            MouseButton::WheelDown,
            MouseModifiers {
                control: true,
                ..Default::default()
            },
            0,
            0,
        )
        .unwrap();
        assert_eq!(wheel, b"\x1b[<81;1;1M");
    }

    #[test]
    fn normal_encoding_uses_offset_bytes() {
        let press = encode(
            CLICK,
            MouseAction::Press,
            MouseButton::Right,
            Default::default(),
            0,
            0,
        )
        .unwrap();
        assert_eq!(press, b"\x1b[M\x22\x21\x21");
        let release = encode(
            CLICK,
            MouseAction::Release,
            MouseButton::Right,
            Default::default(),
            0,
            0,
        )
        .unwrap();
        assert_eq!(release, b"\x1b[M\x23\x21\x21");
        assert_eq!(
            encode(
                CLICK,
                MouseAction::Press,
                MouseButton::Left,
                Default::default(),
                300,
                0
            ),
            None
        );
    }

    #[test]
    fn motion_needs_drag_or_motion_mode() {
        assert_eq!(
            encode(
                CLICK,
                MouseAction::Motion,
                MouseButton::Left,
                Default::default(),
                1,
                1
            ),
            None
        );
        let drag = modes(MouseTracking::Drag, MouseEncoding::Sgr);
        assert_eq!(
            encode(
                drag,
                MouseAction::Motion,
                MouseButton::Left,
                Default::default(),
                1,
                1
            )
            .unwrap(),
            b"\x1b[<32;2;2M"
        );
        assert_eq!(
            encode(
                drag,
                MouseAction::Motion,
                MouseButton::None,
                Default::default(),
                1,
                1
            ),
            None
        );
    }
}
