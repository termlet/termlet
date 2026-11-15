//! 界面大小：设置中的档位（[`crate::model::WindowSettings::ui_scale`]）与系统的文字大小。
//!
//! 界面大小按百分比缩放整个界面（文字、按钮、面板与对话框），与终端的字号无关。设置为 [`SYSTEM`] 时跟随系统：
//! Windows 上是“设置 → 辅助功能 → 文字大小”（注册表 `HKCU\Software\Microsoft\Accessibility` 的 `TextScaleFactor`，
//! 100–225，没改过时没有这一项）。它与显示器的缩放（DPI）是两回事：DPI 由 GPUI 换算，这里不再放大。macOS 没有全局的
//! 文字大小设置；Linux 上 GNOME 的 `text-scaling-factor` 在 X11 下已由 gsd-xsettings 并入 `Xft.dpi`（GPUI 按它算窗口的
//! 缩放），再读一次就放大了两遍，Wayland 下却不并入，分不清，所以这两个系统跟随系统时按 100%。
//!
//! 环境变量 `TERMLET_TEXT_SCALE`（百分比）代替系统的读数：截图、实测“跟随系统”时用，不用改系统设置。

/// 设置中的“跟随系统”。
pub const SYSTEM: u16 = 0;
/// 100%：系统没有文字大小设置、读不到时用它。
pub const DEFAULT: u16 = 100;
/// 设置中可选的固定档位（百分比），从小到大。
pub const PRESETS: [u16; 5] = [90, 100, 110, 125, 150];
/// 最小、最大的界面大小：跟随系统、手改的设置也限制在这之间。系统的文字大小可以到 225%，但 gpui-kit 的页签、标题栏等
/// 有写死像素的高度，超过 150% 后页签上的文字被裁掉，侧边栏、主机工具与终端在常见大小的窗口里也挤不下
pub const MIN: u16 = PRESETS[0];
pub const MAX: u16 = PRESETS[PRESETS.len() - 1];
/// 代替系统读数的环境变量（见模块说明）。
pub const TEXT_SCALE_ENV: &str = "TERMLET_TEXT_SCALE";

/// 实际的界面大小（百分比）：`setting` 为设置中的值（[`SYSTEM`] 为跟随系统），`system` 为系统的文字大小
/// （[`system_text_scale`]）；限制在 [`MIN`]–[`MAX`] 之内。
pub fn effective(setting: u16, system: u16) -> u16 {
    let percent = if setting == SYSTEM { system } else { setting };
    percent.clamp(MIN, MAX)
}

/// 放大一档：比 `current` 大的第一个档位；已是最大时为 [`MAX`]。
pub fn larger(current: u16) -> u16 {
    PRESETS.into_iter().find(|&p| p > current).unwrap_or(MAX)
}

/// 缩小一档：比 `current` 小的第一个档位；已是最小时为 [`MIN`]。
pub fn smaller(current: u16) -> u16 {
    PRESETS
        .into_iter()
        .rev()
        .find(|&p| p < current)
        .unwrap_or(MIN)
}

/// Windows 的 `TextScaleFactor` 换算成百分比：它本来就是百分比（100–225），不在范围内（读到的值不对）时为 100%。
pub fn from_text_scale_factor(raw: u32) -> u16 {
    match u16::try_from(raw) {
        Ok(percent) if (100..=225).contains(&percent) => percent,
        _ => DEFAULT,
    }
}

/// 系统的文字大小（百分比，见模块说明）：有 `TERMLET_TEXT_SCALE` 时用它；Windows 读注册表，没有这一项时为 100%；
/// 其他系统为 100%。没有缓存，每次都读（窗口激活时重读，跟上系统设置的变化）。
pub fn system_text_scale() -> u16 {
    let env = std::env::var(TEXT_SCALE_ENV).ok();
    match env.as_deref().map(str::trim).map(str::parse::<u16>) {
        Some(Ok(percent)) => percent,
        _ => read_system(),
    }
}

#[cfg(windows)]
fn read_system() -> u16 {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Accessibility")
        .and_then(|key| key.get_u32("TextScaleFactor"))
        .map_or(DEFAULT, from_text_scale_factor)
}

#[cfg(not(windows))]
fn read_system() -> u16 {
    DEFAULT
}

#[cfg(test)]
mod tests {
    use super::*;

    // 跟随系统时用系统的读数，固定的档位用设置；都限制在最小、最大之间（手改的设置、系统的 225%）
    #[test]
    fn effective_follows_the_system_or_the_setting() {
        assert_eq!(effective(SYSTEM, 100), 100);
        assert_eq!(effective(SYSTEM, 125), 125);
        assert_eq!(effective(SYSTEM, 113), 113, "系统的读数不必是档位");
        assert_eq!(effective(SYSTEM, 225), MAX);
        assert_eq!(effective(150, 125), 150, "固定的档位不看系统");
        assert_eq!(effective(90, 225), 90);
        assert_eq!(effective(10, 100), MIN);
        assert_eq!(effective(999, 100), MAX);
    }

    // 放大、缩小按档位走；不在档位上（跟随系统的 113%）时走到相邻的档位；到头了不再变
    #[test]
    fn steps_through_the_presets() {
        assert_eq!(larger(100), 110);
        assert_eq!(larger(113), 125);
        assert_eq!(larger(MAX), MAX);
        assert_eq!(smaller(100), 90);
        assert_eq!(smaller(113), 110);
        assert_eq!(smaller(MIN), MIN);
        let mut up = vec![MIN];
        while *up.last().unwrap() != MAX {
            up.push(larger(*up.last().unwrap()));
        }
        assert_eq!(up, PRESETS);
        assert!(PRESETS.contains(&DEFAULT));
        assert!(PRESETS.windows(2).all(|w| w[0] < w[1]), "从小到大");
    }

    // TextScaleFactor 本来就是 100–225 的百分比；超出范围的读数按 100%
    #[test]
    fn text_scale_factor_is_a_percentage() {
        assert_eq!(from_text_scale_factor(100), 100);
        assert_eq!(from_text_scale_factor(125), 125);
        assert_eq!(from_text_scale_factor(225), 225);
        assert_eq!(from_text_scale_factor(0), DEFAULT);
        assert_eq!(from_text_scale_factor(99), DEFAULT);
        assert_eq!(from_text_scale_factor(226), DEFAULT);
        assert_eq!(from_text_scale_factor(u32::MAX), DEFAULT);
    }

    // 系统的读数总在合理的范围内（这台电脑没改过文字大小时是 100%）
    #[test]
    fn system_reading_is_sane() {
        if std::env::var_os(TEXT_SCALE_ENV).is_some() {
            return;
        }
        let percent = system_text_scale();
        assert!((100..=225).contains(&percent), "{percent}");
    }
}
