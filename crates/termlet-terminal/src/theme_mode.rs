//! 深浅色切换通知（DEC 私有模式 2031，contour 提出，kitty、Ghostty、foot 等支持；
//! <https://contour-terminal.org/vt-extensions/color-palette-update-notifications/>）。
//!
//! 程序用 `CSI ? 2031 h` 开启、`CSI ? 2031 l` 关闭。开启后终端配色的背景在深浅之间变了时（界面切换浅色 / 深色、
//! 换配色）终端主动发 `CSI ? 997 ; 1 n`（深色）或 `CSI ? 997 ; 2 n`（浅色），Claude Code、Neovim 等据此跟着切换。
//! 不论是否开启，`CSI ? 996 n` 都回答当前的深浅，`CSI ? 2031 $ p`（DECRQM）回答是否开启。深浅按终端配色的背景
//! 亮度判断，与界面主题无关（主机可以指定固定的配色）；程序用 OSC 11 改过的背景不算。
//!
//! 这属于内核的功能，alacritty 不支持，由 alacritty 后端用这里的状态补上（换 libghostty 后由它提供）：解析前由
//! [`crate::shell_events::OscScanner`] 找出这些序列，引擎在序列之后转给内核，内核更新状态、把回答与其他应答按顺序
//! 写出。alacritty 对不认识的 2031 回答“不支持”，后端丢掉这条回答。RIS（`ESC c`）与换上新的传输时关闭。

use crate::colors::Rgb;

/// 终端配色的深浅。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

impl Theme {
    /// 按背景的相对亮度（WCAG）判断：白字比黑字在它上面更清楚的是深色。
    pub fn of_background(background: Rgb) -> Self {
        let channel = |c: u8| {
            let v = f64::from(c) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let luminance = 0.2126 * channel(background.r)
            + 0.7152 * channel(background.g)
            + 0.0722 * channel(background.b);
        // 与白色的对比度（1.05 / (L + 0.05)）大于与黑色的（(L + 0.05) / 0.05）
        if 1.05 / (luminance + 0.05) > (luminance + 0.05) / 0.05 {
            Theme::Dark
        } else {
            Theme::Light
        }
    }

    /// 通知与查询的回答：`CSI ? 997 ; 1 n`（深色）或 `CSI ? 997 ; 2 n`（浅色）。
    pub fn report(self) -> String {
        let code = match self {
            Theme::Dark => 1,
            Theme::Light => 2,
        };
        format!("\x1b[?997;{code}n")
    }
}

/// 程序的一个请求。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeRequest {
    /// `CSI ? 2031 h`（也可以与别的模式写在一起，如 `CSI ? 1049 ; 2031 h`）
    Enable,
    /// `CSI ? 2031 l`
    Disable,
    /// `CSI ? 996 n`：问当前的深浅
    Query,
    /// `CSI ? 2031 $ p`（DECRQM）：问是否开启
    ModeQuery,
}

impl ThemeRequest {
    /// 由 CSI 的参数部分（结尾字符之前，含 `?` 与中间字节）与结尾字符解析；不是这里的请求时为 `None`。
    pub fn parse(params: &[u8], final_byte: u8) -> Option<Self> {
        let rest = params.strip_prefix(b"?")?;
        match final_byte {
            b'h' | b'l' => {
                if !rest.iter().all(|b| b.is_ascii_digit() || *b == b';') {
                    return None;
                }
                rest.split(|&b| b == b';')
                    .any(|mode| mode == b"2031")
                    .then_some(if final_byte == b'h' {
                        ThemeRequest::Enable
                    } else {
                        ThemeRequest::Disable
                    })
            }
            b'n' => (rest == b"996").then_some(ThemeRequest::Query),
            b'p' => (rest == b"2031$").then_some(ThemeRequest::ModeQuery),
            _ => None,
        }
    }
}

/// 通知是否开启与终端配色当前的深浅。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThemeReports {
    enabled: bool,
    theme: Theme,
}

impl ThemeReports {
    /// 程序是否开启了通知。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 处理程序的请求；要回答时返回写回远端的内容。
    pub fn apply(&mut self, request: ThemeRequest) -> Option<String> {
        match request {
            ThemeRequest::Enable => self.enabled = true,
            ThemeRequest::Disable => self.enabled = false,
            ThemeRequest::Query => return Some(self.theme.report()),
            ThemeRequest::ModeQuery => {
                let state = if self.enabled { 1 } else { 2 };
                return Some(format!("\x1b[?2031;{state}$y"));
            }
        }
        None
    }

    /// 终端配色换了：深浅变了且开启着通知时返回要主动发出的通知。
    pub fn set_theme(&mut self, theme: Theme) -> Option<String> {
        let changed = self.theme != theme;
        self.theme = theme;
        (changed && self.enabled).then(|| theme.report())
    }

    /// 关闭通知（RIS、换上新的传输）；配色的深浅照旧。
    pub fn reset(&mut self) {
        self.enabled = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(hex: u32) -> Rgb {
        Rgb {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
        }
    }

    #[test]
    fn theme_by_background_luminance() {
        for dark in [0x000000, 0x1e1e1e, 0x282c34, 0x002b36, 0x1e1e2e, 0x5a5a5a] {
            assert_eq!(Theme::of_background(rgb(dark)), Theme::Dark, "{dark:06x}");
        }
        for light in [0xffffff, 0xfdf6e3, 0xeff1f5, 0xfafafa, 0xa0a0a0] {
            assert_eq!(
                Theme::of_background(rgb(light)),
                Theme::Light,
                "{light:06x}"
            );
        }
        assert_eq!(Theme::Dark.report(), "\x1b[?997;1n");
        assert_eq!(Theme::Light.report(), "\x1b[?997;2n");
    }

    #[test]
    fn parses_requests() {
        let parse =
            |params: &str, final_byte: u8| ThemeRequest::parse(params.as_bytes(), final_byte);
        assert_eq!(parse("?2031", b'h'), Some(ThemeRequest::Enable));
        assert_eq!(parse("?2031", b'l'), Some(ThemeRequest::Disable));
        assert_eq!(parse("?1049;2031", b'h'), Some(ThemeRequest::Enable));
        assert_eq!(parse("?996", b'n'), Some(ThemeRequest::Query));
        assert_eq!(parse("?2031$", b'p'), Some(ThemeRequest::ModeQuery));
        for (params, final_byte) in [
            ("2031", b'h'),
            ("?20310", b'h'),
            ("?1049", b'h'),
            ("?2031:1", b'h'),
            ("?2031", b'n'),
            ("?996", b'h'),
            ("?6", b'n'),
            ("?2031", b'p'),
            ("2031$", b'p'),
            ("?2026$", b'p'),
        ] {
            assert_eq!(
                parse(params, final_byte),
                None,
                "{params} {}",
                final_byte as char
            );
        }
    }

    // 开启后深浅变了才通知（同样的深浅不重复）；查询不论开没开都回答；关闭、重置后不再通知
    #[test]
    fn reports_follow_the_mode() {
        let mut reports = ThemeReports::default();
        assert_eq!(reports.set_theme(Theme::Light), None, "没有开启");
        assert_eq!(
            reports.apply(ThemeRequest::Query).as_deref(),
            Some("\x1b[?997;2n")
        );
        assert_eq!(
            reports.apply(ThemeRequest::ModeQuery).as_deref(),
            Some("\x1b[?2031;2$y")
        );
        assert_eq!(reports.apply(ThemeRequest::Enable), None);
        assert!(reports.enabled());
        assert_eq!(
            reports.apply(ThemeRequest::ModeQuery).as_deref(),
            Some("\x1b[?2031;1$y")
        );
        assert_eq!(reports.set_theme(Theme::Light), None, "深浅没变");
        assert_eq!(
            reports.set_theme(Theme::Dark).as_deref(),
            Some("\x1b[?997;1n")
        );
        assert_eq!(reports.apply(ThemeRequest::Disable), None);
        assert_eq!(reports.set_theme(Theme::Light), None);
        reports.apply(ThemeRequest::Enable);
        reports.reset();
        assert!(!reports.enabled());
        assert_eq!(reports.set_theme(Theme::Dark), None);
        assert_eq!(
            reports.apply(ThemeRequest::Query).as_deref(),
            Some("\x1b[?997;1n"),
            "重置后深浅照旧"
        );
    }
}
